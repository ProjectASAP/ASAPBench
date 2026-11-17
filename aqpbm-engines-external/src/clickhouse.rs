//! Translates a plan file into ClickHouse: one MergeTree table per metric,
//! one AggregatingMergeTree table and materialized view per stored window
//! chain, and one SQL query per query root, evaluated at the query parameter
//! `{t:Int64}` (epoch milliseconds).
//!
//! The translator is the capability check. A first pass, in node id order,
//! refuses every operator and field it has no rule for on the node that
//! carries it; then each root is translated from these node patterns, and any
//! other pattern is refused:
//!
//! * a stored chain of exact Sum/Count/Min/Max/Rate/Increase, per series,
//!   over an ingestion-time Scan, read by `TimeRange` + `SummaryMerge` +
//!   `FinalizeExactAccumulator`;
//! * a query-time `SummaryAgg` of the same kinds, per series, over
//!   `TimeRange` + `Scan`, then `FinalizeExactAccumulator`;
//! * `Aggregate` (`sum`, `count`, `min`, `max`, `avg`, `quantile`) over a
//!   query-time `TimeRange` + `Scan` or over another pattern.
//!
//! Windows are right-closed, `(t − range, t]`, like the planner's. A query
//! that reads stored windows is right only at a `t` that is a window end:
//! [`ClickHouseDeployment::query_time_steps_ms`] gives the step `t` must be a
//! multiple of. Rate and increase follow Prometheus `extrapolatedRate`
//! (`promql/functions.go`, v3.15.1).
//!
//! A stored counter chain keeps `deltaSumTimestamp` states, which add 0 for
//! the step across a counter reset (a [`Substitution`]). They also need each
//! series' rows inserted in time order, so that the states of one window do
//! not overlap in time: insert time-ordered chunks, one after another.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::plan_file::{
    Lifetime, Measure, PlanFile, PlanNode, PlanOperator, Predicate, RangeKind, Reduction, Source,
    SummaryFamily, Timing,
};
use crate::{NodeId, SharingLost, Substitution, TranslationError};

/// The plan's series identity column, a label column like any other.
const SERIES_IDENTITY: &str = "$promql_series_identity";

/// What a plan becomes on ClickHouse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickHouseDeployment {
    /// `CREATE TABLE` statements: one per metric, then one per stored chain.
    pub tables: Vec<String>,
    /// `CREATE MATERIALIZED VIEW` statements, one per stored chain.
    pub materialized_views: Vec<String>,
    /// One `SELECT` per query root, in root order, with the parameter `{t:Int64}`.
    pub queries: Vec<String>,
    /// Per query, the step its evaluation time `t` must be a multiple of: the
    /// window width of the stored chains it reads (1 when it reads none).
    pub query_time_steps_ms: Vec<u64>,
    pub substitutions: Vec<Substitution>,
    pub sharing_lost: Vec<SharingLost>,
}

impl ClickHouseDeployment {
    /// Every statement, in order (tables, views, queries), each ending with `;`.
    pub fn script(&self) -> String {
        self.tables
            .iter()
            .chain(&self.materialized_views)
            .chain(&self.queries)
            .map(|statement| format!("{statement};\n\n"))
            .collect()
    }
}

/// Translates `plan`; the first node with no rule is refused.
pub fn translate(plan: &PlanFile) -> Result<ClickHouseDeployment, TranslationError> {
    for node in &plan.nodes {
        refuse_unsupported_fields(node)?;
    }
    let mut translator = Translator {
        plan,
        chains: BTreeMap::new(),
        query_time_step_ms: 1,
    };
    let mut queries = Vec::new();
    let mut query_time_steps_ms = Vec::new();
    for &root in &plan.roots {
        translator.query_time_step_ms = 1;
        queries.push(translator.root_query(root)?);
        query_time_steps_ms.push(translator.query_time_step_ms);
    }
    let mut tables = source_tables(plan);
    let mut materialized_views = Vec::new();
    let mut substitutions = Vec::new();
    for chain in translator.chains.values() {
        tables.push(chain.table());
        materialized_views.push(chain.view());
        if chain.kind.is_counter() {
            substitutions.push(Substitution {
                node: chain.node,
                rule: "clickhouse.stored_rate.delta_sum_timestamp".into(),
                guarantee_difference: "a counter reset adds 0 for the step across it, where \
                                       PromQL adds the first value after the reset"
                    .into(),
            });
        }
    }
    Ok(ClickHouseDeployment {
        tables,
        materialized_views,
        queries,
        query_time_steps_ms,
        substitutions,
        sharing_lost: sharing_lost(plan),
    })
}

fn unsupported(node: NodeId, reason: impl Into<String>) -> TranslationError {
    TranslationError::UnsupportedShape {
        node,
        reason: reason.into(),
    }
}

/// The first pass: an operator or field with no rule, on the node carrying it.
fn refuse_unsupported_fields(node: &PlanNode) -> Result<(), TranslationError> {
    let refuse = |reason: &str| Err(unsupported(node.id, reason));
    if node.timing == Timing::QueryTime && node.lifetime == Lifetime::AcrossEvaluations {
        return refuse("query-time result kept across evaluations");
    }
    let refuse_without = |reduction: &Reduction| match reduction {
        Reduction::Reduce { without: true, .. } => refuse("without"),
        _ => Ok(()),
    };
    match &node.operator {
        PlanOperator::Other { kind } => refuse(&format!("operator {kind}")),
        PlanOperator::Scan { source, predicates } => {
            if *source == Source::Table {
                return refuse("table source");
            }
            if predicates.iter().any(|p| matches!(p, Predicate::Other(_))) {
                return refuse("predicate");
            }
            Ok(())
        }
        PlanOperator::WindowChain {
            family,
            reduction,
            input,
            filter,
            ..
        }
        | PlanOperator::SummaryAgg {
            family,
            reduction,
            input,
            filter,
            ..
        } => {
            if filter.is_some() {
                return refuse("filter");
            }
            if *input != serde_json::json!({"item": null, "weight": {"Column": "SampleValue"}}) {
                return refuse("summary input");
            }
            refuse_without(reduction)?;
            match family {
                SummaryFamily::Exact(kind) if ExactKind::of(kind).is_some() => Ok(()),
                SummaryFamily::Exact(kind) => refuse(&format!("exact {kind}")),
                SummaryFamily::Sketch(algorithm) => refuse(&format!("sketch {algorithm}")),
            }
        }
        PlanOperator::SummaryMerge { group_by, .. } => match group_by {
            Some(_) => refuse("regrouping merge"),
            None => Ok(()),
        },
        PlanOperator::Aggregate {
            filters,
            having,
            reduction,
            ..
        } => {
            if !filters.is_empty() {
                return refuse("filter");
            }
            if having.is_some() {
                return refuse("having");
            }
            refuse_without(reduction)
        }
        PlanOperator::TimeRange { kind, .. } => match kind {
            RangeKind::Range => Ok(()),
            RangeKind::Instant => refuse("instant selector"),
        },
        PlanOperator::Finalize { .. } => Ok(()),
        PlanOperator::SummaryEstimate { .. } => refuse("summary estimate"),
    }
}

/// The exact kinds this translator builds, with their ClickHouse functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExactKind {
    Sum,
    Count,
    Min,
    Max,
    Rate,
    Increase,
}

impl ExactKind {
    fn of(kind: &str) -> Option<ExactKind> {
        Some(match kind {
            "Sum" => ExactKind::Sum,
            "Count" => ExactKind::Count,
            "Min" => ExactKind::Min,
            "Max" => ExactKind::Max,
            "Rate" => ExactKind::Rate,
            "Increase" => ExactKind::Increase,
            _ => return None,
        })
    }

    fn is_counter(self) -> bool {
        matches!(self, ExactKind::Rate | ExactKind::Increase)
    }

    /// The aggregate function of a plain kind (`None` for a counter kind).
    fn function(self) -> Option<&'static str> {
        match self {
            ExactKind::Sum => Some("sum"),
            ExactKind::Count => Some("count"),
            ExactKind::Min => Some("min"),
            ExactKind::Max => Some("max"),
            ExactKind::Rate | ExactKind::Increase => None,
        }
    }
}

/// `f(value)`, or `count()`: `f`'s call over the samples' values.
fn over_values(function: &str) -> String {
    match function {
        "count" => "count()".into(),
        _ => format!("{function}(value)"),
    }
}

/// One `CREATE TABLE` per metric, in order of first Scan, over the union of
/// its Scans' label columns: in name order, the series identity last.
fn source_tables(plan: &PlanFile) -> Vec<String> {
    let mut metrics: Vec<(&str, Vec<&str>)> = Vec::new();
    for node in &plan.nodes {
        let PlanOperator::Scan {
            source: Source::TimeSeries { metric },
            ..
        } = &node.operator
        else {
            continue;
        };
        let at = match metrics.iter().position(|(m, _)| m == metric) {
            Some(at) => at,
            None => {
                metrics.push((metric, Vec::new()));
                metrics.len() - 1
            }
        };
        for label in &node.label_columns {
            if !metrics[at].1.contains(&label.as_str()) {
                metrics[at].1.push(label);
            }
        }
    }
    metrics
        .into_iter()
        .map(|(metric, mut labels)| {
            labels.sort_by_key(|l| (*l == SERIES_IDENTITY, *l));
            let columns: String = labels
                .iter()
                .map(|label| format!(", {} LowCardinality(String)", quoted(label)))
                .collect();
            format!(
                "CREATE TABLE {} (ts DateTime64(3, 'UTC'), value Float64{columns}) \
                 ENGINE = MergeTree ORDER BY (ts, {})",
                source_table(metric),
                quoted(SERIES_IDENTITY)
            )
        })
        .collect()
}

/// Query-time nodes two or more readers share.
fn sharing_lost(plan: &PlanFile) -> Vec<SharingLost> {
    plan.nodes
        .iter()
        .filter(|node| node.timing == Timing::QueryTime)
        .filter_map(|node| {
            let readers = plan.readers_of(node.id);
            (readers.len() >= 2).then_some(SharingLost {
                node: node.id,
                readers,
            })
        })
        .collect()
}

/// A query relation: SQL whose result has the label columns, then `value`.
struct Relation {
    sql: String,
    labels: Vec<String>,
}

/// Raw samples of a metric in `(t − range, t]`, filtered by labels.
struct RawSamples {
    table: String,
    labels: Vec<String>,
    where_sql: String,
}

/// A stored window chain: its table `v<node>` and view `v<node>_mv`.
struct StoredChain {
    node: NodeId,
    kind: ExactKind,
    labels: Vec<String>,
    width_ms: u64,
    source_table: String,
    /// The chain's Scan predicates, as SQL conditions (empty when none).
    where_sql: String,
}

impl StoredChain {
    fn table(&self) -> String {
        let label_columns: String = self
            .labels
            .iter()
            .map(|label| format!("{} LowCardinality(String), ", quoted(label)))
            .collect();
        let states = match self.kind.function() {
            None => "s_count AggregateFunction(count), s_first_ms AggregateFunction(min, Int64), \
                     s_last_ms AggregateFunction(max, Int64), \
                     s_first_value AggregateFunction(argMin, Float64, Int64), \
                     s_increase AggregateFunction(deltaSumTimestamp, Float64, Int64)"
                .to_string(),
            Some("count") => "state AggregateFunction(count)".to_string(),
            Some(function) => format!("state AggregateFunction({function}, Float64)"),
        };
        format!(
            "CREATE TABLE v{} ({label_columns}window_end_ms Int64, {states}) \
             ENGINE = AggregatingMergeTree ORDER BY (window_end_ms, {})",
            self.node,
            column_list(&self.labels)
        )
    }

    fn view(&self) -> String {
        let ts_ms = "toUnixTimestamp64Milli(ts)";
        let width = self.width_ms;
        let group = column_list(&self.labels);
        let states = match self.kind.function() {
            None => format!(
                "countState() AS s_count, minState({ts_ms}) AS s_first_ms, \
                 maxState({ts_ms}) AS s_last_ms, argMinState(value, {ts_ms}) AS s_first_value, \
                 deltaSumTimestampState(value, {ts_ms}) AS s_increase"
            ),
            Some("count") => "countState() AS state".to_string(),
            Some(function) => format!("{function}State(value) AS state"),
        };
        let filter = if self.where_sql.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.where_sql)
        };
        format!(
            "CREATE MATERIALIZED VIEW v{node}_mv TO v{node} AS SELECT {group}, \
             (intDiv({ts_ms} - 1, {width}) + 1) * {width} AS window_end_ms, {states} \
             FROM {table}{filter} GROUP BY {group}, window_end_ms",
            node = self.node,
            table = self.source_table,
        )
    }
}

struct Translator<'a> {
    plan: &'a PlanFile,
    /// The stored chains the queries read, by node.
    chains: BTreeMap<NodeId, StoredChain>,
    /// The step the current query's `t` must be a multiple of.
    query_time_step_ms: u64,
}

impl Translator<'_> {
    fn node(&self, id: NodeId) -> &PlanNode {
        &self.plan.nodes[id]
    }

    fn root_query(&mut self, root: NodeId) -> Result<String, TranslationError> {
        Ok(self.relation(root)?.sql)
    }

    fn relation(&mut self, id: NodeId) -> Result<Relation, TranslationError> {
        let plan = self.plan;
        match &plan.nodes[id].operator {
            PlanOperator::Finalize { child } => self.finalized(id, *child),
            PlanOperator::Aggregate {
                child,
                measures,
                reduction,
                ..
            } => self.aggregate(id, *child, measures, reduction),
            _ => Err(unsupported(id, "this operator as a query result")),
        }
    }

    /// `range`, a `TimeRange` over a query-time time-series Scan.
    fn raw_samples(&self, range: NodeId) -> Result<RawSamples, TranslationError> {
        let PlanOperator::TimeRange {
            child: scan,
            range_ms,
            ..
        } = self.node(range).operator
        else {
            return Err(unsupported(range, "this operator over raw samples"));
        };
        let scan_node = self.node(scan);
        let PlanOperator::Scan {
            source: Source::TimeSeries { metric },
            ..
        } = &scan_node.operator
        else {
            return Err(unsupported(range, "time range over this operator"));
        };
        if scan_node.timing != Timing::QueryTime {
            return Err(unsupported(range, "time range over an ingestion-time scan"));
        }
        let mut conditions = self.label_conditions(scan)?;
        conditions.push(format!(
            "ts > fromUnixTimestamp64Milli({{t:Int64}} - {range_ms}) \
             AND ts <= fromUnixTimestamp64Milli({{t:Int64}})"
        ));
        Ok(RawSamples {
            table: source_table(metric),
            labels: self.node(range).label_columns.clone(),
            where_sql: conditions.join(" AND "),
        })
    }

    /// The label-equality predicates of Scan `scan`, as SQL conditions.
    fn label_conditions(&self, scan: NodeId) -> Result<Vec<String>, TranslationError> {
        let scan_node = self.node(scan);
        let PlanOperator::Scan { predicates, .. } = &scan_node.operator else {
            return Ok(Vec::new());
        };
        predicates
            .iter()
            .map(|predicate| match predicate {
                Predicate::LabelEquals { column, value } => {
                    let name = scan_node
                        .output_columns
                        .get(*column)
                        .filter(|name| scan_node.label_columns.contains(name))
                        .ok_or_else(|| unsupported(scan, "predicate on a non-label column"))?;
                    Ok(format!("{} = {}", quoted(name), string_literal(value)))
                }
                Predicate::Other(_) => Err(unsupported(scan, "predicate")),
            })
            .collect()
    }

    /// `FinalizeExactAccumulator` over a stored chain's merged windows or over
    /// a query-time `SummaryAgg`.
    fn finalized(&mut self, id: NodeId, child: NodeId) -> Result<Relation, TranslationError> {
        let plan = self.plan;
        match &plan.nodes[child].operator {
            PlanOperator::SummaryMerge { children, .. } => {
                let [range] = children[..] else {
                    return Err(unsupported(child, "merge of several inputs"));
                };
                let PlanOperator::TimeRange {
                    child: chain,
                    range_ms,
                    ..
                } = self.node(range).operator
                else {
                    return Err(unsupported(child, "merge of this operator"));
                };
                self.stored_read(range, chain, range_ms)
            }
            PlanOperator::SummaryAgg {
                child: range,
                family: SummaryFamily::Exact(kind),
                reduction,
                ..
            } => {
                if *reduction != Reduction::PerEntity {
                    return Err(unsupported(child, "grouped summary"));
                }
                let kind = ExactKind::of(kind).ok_or_else(|| unsupported(child, "exact kind"))?;
                let PlanOperator::TimeRange { range_ms, .. } = self.node(*range).operator else {
                    return Err(unsupported(child, "summary over this operator"));
                };
                let raw = self.raw_samples(*range)?;
                let group = column_list(&raw.labels);
                let sql = match kind.function() {
                    Some(function) => format!(
                        "SELECT {group}, {} AS value FROM {} WHERE {} GROUP BY {group}",
                        over_values(function),
                        raw.table,
                        raw.where_sql
                    ),
                    None => {
                        let inner = format!(
                            "SELECT {group}, \
                             arraySort(groupArray((toUnixTimestamp64Milli(ts), value))) AS points, \
                             arrayMap(p -> p.2, points) AS vals, \
                             length(points) AS n, \
                             points[1].1 AS first_ms, \
                             points[n].1 AS last_ms, \
                             vals[1] AS v_first, \
                             vals[n] - vals[1] + arraySum(arrayMap((cur, prev) -> \
                             if(cur < prev, prev, 0), arrayPopFront(vals), arrayPopBack(vals))) \
                             AS increase \
                             FROM {} WHERE {} GROUP BY {group} HAVING n >= 2",
                            raw.table, raw.where_sql
                        );
                        extrapolated(&group, &inner, range_ms, kind)
                    }
                };
                Ok(Relation {
                    sql,
                    labels: raw.labels,
                })
            }
            _ => Err(unsupported(id, "finalize over this operator")),
        }
    }

    /// The merged windows of stored chain `chain` in `(t − range_ms, t]`,
    /// read by `TimeRange` node `range`.
    fn stored_read(
        &mut self,
        range: NodeId,
        chain: NodeId,
        range_ms: u64,
    ) -> Result<Relation, TranslationError> {
        let stored = self.stored_chain(chain)?;
        if !range_ms.is_multiple_of(stored.width_ms) {
            return Err(unsupported(
                range,
                "a range that is not a whole number of windows",
            ));
        }
        self.query_time_step_ms = lcm(self.query_time_step_ms, stored.width_ms);
        let group = column_list(&stored.labels);
        let window =
            format!("window_end_ms > {{t:Int64}} - {range_ms} AND window_end_ms <= {{t:Int64}}");
        let sql = match stored.kind.function() {
            Some(function) => format!(
                "SELECT {group}, {function}Merge(state) AS value FROM v{chain} \
                 WHERE {window} GROUP BY {group}"
            ),
            None => {
                let inner = format!(
                    "SELECT {group}, countMerge(s_count) AS n, minMerge(s_first_ms) AS first_ms, \
                     maxMerge(s_last_ms) AS last_ms, argMinMerge(s_first_value) AS v_first, \
                     deltaSumTimestampMerge(s_increase) AS increase \
                     FROM v{chain} WHERE {window} GROUP BY {group} HAVING n >= 2"
                );
                extrapolated(&group, &inner, range_ms, stored.kind)
            }
        };
        let labels = stored.labels.clone();
        self.chains.insert(chain, stored);
        Ok(Relation { sql, labels })
    }

    /// The stored chain at `chain`, if this translator supports it:
    /// ingestion-time, kept across evaluations, per series, over an
    /// ingestion-time time-series Scan.
    fn stored_chain(&self, chain: NodeId) -> Result<StoredChain, TranslationError> {
        let node = self.node(chain);
        let PlanOperator::WindowChain {
            child,
            family: SummaryFamily::Exact(kind),
            reduction,
            width_ms,
            ..
        } = &node.operator
        else {
            return Err(unsupported(chain, "time range over this operator"));
        };
        let kind = ExactKind::of(kind).ok_or_else(|| unsupported(chain, "exact kind"))?;
        if *reduction != Reduction::PerEntity {
            return Err(unsupported(chain, "grouped window chain"));
        }
        if *width_ms == 0 {
            return Err(unsupported(chain, "window width 0"));
        }
        if node.timing != Timing::IngestionTime || node.lifetime != Lifetime::AcrossEvaluations {
            return Err(unsupported(chain, "window chain not stored at ingestion"));
        }
        let scan = self.node(*child);
        let (
            PlanOperator::Scan {
                source: Source::TimeSeries { metric },
                ..
            },
            Timing::IngestionTime,
        ) = (&scan.operator, scan.timing)
        else {
            return Err(unsupported(chain, "window chain over this operator"));
        };
        Ok(StoredChain {
            node: chain,
            kind,
            labels: node.label_columns.clone(),
            width_ms: *width_ms,
            source_table: source_table(metric),
            where_sql: self.label_conditions(*child)?.join(" AND "),
        })
    }

    fn aggregate(
        &mut self,
        id: NodeId,
        child: NodeId,
        measures: &[Measure],
        reduction: &Reduction,
    ) -> Result<Relation, TranslationError> {
        let [measure] = measures else {
            return Err(unsupported(id, "several measures"));
        };
        let value = aggregate_value(id, measure)?;
        let (from, input_labels) = match self.node(child).operator {
            PlanOperator::TimeRange { .. } => {
                let raw = self.raw_samples(child)?;
                (format!("{} WHERE {}", raw.table, raw.where_sql), raw.labels)
            }
            _ => {
                let input = self.relation(child)?;
                (format!("( {})", input.sql), input.labels)
            }
        };
        let labels = match reduction {
            Reduction::PerEntity => input_labels,
            Reduction::Reduce { columns, .. } => {
                let names = &self.node(child).output_columns;
                columns
                    .iter()
                    .map(|&column| {
                        names
                            .get(column)
                            .filter(|name| input_labels.contains(name))
                            .cloned()
                            .ok_or_else(|| unsupported(id, "group column"))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
        };
        let sql = if labels.is_empty() {
            format!("SELECT {value} AS value FROM {from} HAVING count() > 0")
        } else {
            let group = column_list(&labels);
            format!("SELECT {group}, {value} AS value FROM {from} GROUP BY {group}")
        };
        Ok(Relation { sql, labels })
    }
}

/// The SQL of one Aggregate measure over the input's `value`.
fn aggregate_value(id: NodeId, measure: &Measure) -> Result<String, TranslationError> {
    let extra_arguments = |allowed: &[&str]| {
        measure
            .arguments
            .keys()
            .any(|key| !allowed.contains(&key.as_str()))
    };
    if measure.column.is_some() {
        return Err(unsupported(id, "measure over a column"));
    }
    match measure.kind.as_str() {
        "sum" | "count" | "min" | "max" | "avg" if !extra_arguments(&[]) => {
            Ok(over_values(&measure.kind))
        }
        "quantile" if !extra_arguments(&["q"]) => {
            let q = measure
                .arguments
                .get("q")
                .and_then(Value::as_f64)
                .filter(|q| (0.0..=1.0).contains(q))
                .ok_or_else(|| unsupported(id, "quantile level"))?;
            Ok(format!("quantileExactInclusive({q})(value)"))
        }
        kind => Err(unsupported(id, format!("measure {kind}"))),
    }
}

/// Prometheus `extrapolatedRate` (`promql/functions.go`, v3.15.1) over
/// `inner`, which gives per series `n`, `first_ms`, `last_ms`, `v_first` and
/// the reset-corrected `increase` of the samples in `(t − range_ms, t]`
/// (at least 2): the 1.1 × average-gap threshold first, then the zero-point
/// clamp; divided by the range in seconds for a rate.
fn extrapolated(group: &str, inner: &str, range_ms: u64, kind: ExactKind) -> String {
    let per_second = if kind == ExactKind::Rate {
        format!(" / {}", ms_as_seconds_literal(range_ms))
    } else {
        String::new()
    };
    format!(
        "SELECT {group}, increase * (sampled + least(d_start, d_zero) + d_end) / sampled{per_second} \
         AS value FROM ( SELECT {group}, increase, (last_ms - first_ms) / 1000 AS sampled, \
         sampled / (n - 1) AS avg_gap, \
         if((first_ms - ({{t:Int64}} - {range_ms})) / 1000 >= 1.1 * avg_gap, avg_gap / 2, \
         (first_ms - ({{t:Int64}} - {range_ms})) / 1000) AS d_start, \
         if(increase > 0 AND v_first >= 0, sampled * v_first / increase, d_start) AS d_zero, \
         if(({{t:Int64}} - last_ms) / 1000 >= 1.1 * avg_gap, avg_gap / 2, \
         ({{t:Int64}} - last_ms) / 1000) AS d_end \
         FROM ( {inner}))"
    )
}

/// `300000` ms as `300`; a fraction of a second keeps its decimals.
fn ms_as_seconds_literal(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        (ms / 1000).to_string()
    } else {
        format!("{}", ms as f64 / 1000.0)
    }
}

fn lcm(a: u64, b: u64) -> u64 {
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    a / x * b
}

fn source_table(metric: &str) -> String {
    let name: String = metric
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("src_{name}")
}

fn column_list(labels: &[String]) -> String {
    labels
        .iter()
        .map(|l| quoted(l))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quoted(identifier: &str) -> String {
    format!("`{}`", identifier.replace('\\', "\\\\").replace('`', "\\`"))
}

fn string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_keep_fractions() {
        assert_eq!(ms_as_seconds_literal(300_000), "300");
        assert_eq!(ms_as_seconds_literal(1_500), "1.5");
    }

    #[test]
    fn identifiers_and_literals_are_escaped() {
        assert_eq!(quoted("a`b"), "`a\\`b`");
        assert_eq!(string_literal("it's"), "'it\\'s'");
    }

    #[test]
    fn lcm_of_window_widths() {
        assert_eq!(lcm(1, 60_000), 60_000);
        assert_eq!(lcm(60_000, 90_000), 180_000);
    }
}

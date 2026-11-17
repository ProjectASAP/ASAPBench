//! Translates a plan file into ClickHouse: one MergeTree table per metric,
//! one AggregatingMergeTree table and materialized view per stored window
//! chain, and one SQL query per query root, evaluated at the query parameter
//! `{t:Int64}` (epoch milliseconds).
//!
//! The translator is the capability check. A first pass, in node id order,
//! refuses every operator and field it has no rule for on the node that
//! carries it; then each root is translated from its node patterns:
//!
//! * a stored chain of exact Sum/Count/Min/Max/Rate/Increase, read by
//!   `TimeRange` + `SummaryMerge` + `FinalizeExactAccumulator`;
//! * a query-time `SummaryAgg` of the same kinds over `TimeRange` + `Scan`,
//!   then `FinalizeExactAccumulator`;
//! * `Aggregate` (`sum`, `count`, `min`, `max`, `avg`, `quantile`) over a
//!   query-time `TimeRange` + `Scan` or over another pattern.
//!
//! Windows are right-closed, `(t − range, t]`, like the planner's. Rate and
//! increase follow Prometheus `extrapolatedRate` (`promql/functions.go`,
//! v3.15.1). A stored counter chain uses `deltaSumTimestamp`, which adds 0
//! for the step across a counter reset: a [`Substitution`].

use serde_json::{json, Value};

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
        substitutions: Vec::new(),
        chains: Vec::new(),
    };
    let queries = plan
        .roots
        .iter()
        .map(|&root| translator.root_query(root))
        .collect::<Result<Vec<_>, _>>()?;
    let mut tables = source_tables(plan)?;
    let mut materialized_views = Vec::new();
    let mut chains = translator.chains.clone();
    chains.sort_unstable();
    chains.dedup();
    for chain in chains {
        let (table, view) = translator.chain_ddl(chain)?;
        tables.push(table);
        materialized_views.push(view);
    }
    let mut substitutions = translator.substitutions;
    substitutions.sort_by_key(|s| s.node);
    substitutions.dedup();
    Ok(ClickHouseDeployment {
        tables,
        materialized_views,
        queries,
        substitutions,
        sharing_lost: sharing_lost(plan),
    })
}

/// The first pass: an operator or field with no rule, on the node carrying it.
fn refuse_unsupported_fields(node: &PlanNode) -> Result<(), TranslationError> {
    let refuse = |reason: &str| {
        Err(TranslationError::UnsupportedShape {
            node: node.id,
            reason: reason.into(),
        })
    };
    let refuse_reduction = |reduction: &Reduction| match reduction {
        Reduction::Reduce { without: true, .. } => refuse("without"),
        _ => Ok(()),
    };
    let summary_input = json!({"item": null, "weight": {"Column": "SampleValue"}});
    match &node.operator {
        PlanOperator::Other { kind } => refuse(&format!("operator {kind}")),
        PlanOperator::Scan { source, predicates } => {
            if matches!(source, Source::Table { .. }) {
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
            if *input != summary_input {
                return refuse("summary input");
            }
            refuse_reduction(reduction)?;
            match family {
                SummaryFamily::Exact(kind)
                    if counter_kind(kind).is_some() || plain_kind(kind).is_some() =>
                {
                    Ok(())
                }
                SummaryFamily::Exact(kind) => refuse(&format!("exact {kind}")),
                SummaryFamily::Sketch(algorithm) => refuse(&format!("sketch {algorithm}")),
            }
        }
        PlanOperator::SummaryMerge { group_by, .. } => {
            if group_by.is_some() {
                return refuse("regrouping merge");
            }
            Ok(())
        }
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
            refuse_reduction(reduction)
        }
        PlanOperator::TimeRange { kind, .. } => match kind {
            RangeKind::Range => Ok(()),
            RangeKind::Instant => refuse("instant selector"),
        },
        PlanOperator::Finalize { .. } => Ok(()),
        PlanOperator::SummaryEstimate { .. } => refuse("summary estimate"),
    }
}

/// The ClickHouse aggregate of a plain exact kind.
fn plain_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "Sum" => Some("sum"),
        "Count" => Some("count"),
        "Min" => Some("min"),
        "Max" => Some("max"),
        _ => None,
    }
}

/// `Some(true)` for a rate, `Some(false)` for an increase.
fn counter_kind(kind: &str) -> Option<bool> {
    match kind {
        "Rate" => Some(true),
        "Increase" => Some(false),
        _ => None,
    }
}

/// One `CREATE TABLE` per metric, in order of first Scan, over the union of
/// its Scans' label columns: in name order, the series identity last.
fn source_tables(plan: &PlanFile) -> Result<Vec<String>, TranslationError> {
    let mut metrics: Vec<(String, Vec<String>)> = Vec::new();
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
                metrics.push((metric.clone(), Vec::new()));
                metrics.len() - 1
            }
        };
        for column in labels_of(&node.output_columns) {
            if !metrics[at].1.contains(&column) {
                metrics[at].1.push(column);
            }
        }
    }
    Ok(metrics
        .into_iter()
        .map(|(metric, mut labels)| {
            labels.sort_by_key(|l| (l == SERIES_IDENTITY, l.clone()));
            let columns: String = labels
                .iter()
                .map(|label| format!(", {} String", quoted(label)))
                .collect();
            format!(
                "CREATE TABLE {} (ts DateTime64(3, 'UTC'), value Float64{columns}) \
                 ENGINE = MergeTree ORDER BY (ts, {})",
                source_table(&metric),
                quoted(SERIES_IDENTITY)
            )
        })
        .collect())
}

/// Query-time nodes two or more readers share.
fn sharing_lost(plan: &PlanFile) -> Vec<SharingLost> {
    plan.nodes
        .iter()
        .filter(|node| {
            node.timing == Timing::QueryTime && node.lifetime != Lifetime::AcrossEvaluations
        })
        .filter_map(|node| {
            let readers = plan.readers_of(node.id);
            (readers.len() >= 2).then_some(SharingLost {
                node: node.id,
                readers,
            })
        })
        .collect()
}

/// A relation a query reads: label columns, then `value`.
enum Relation {
    /// Raw samples of a metric in `(t − range, t]`, filtered.
    Raw {
        table: String,
        labels: Vec<String>,
        conditions: String,
    },
    /// A subquery with these label columns.
    Query { sql: String, labels: Vec<String> },
}

struct Translator<'a> {
    plan: &'a PlanFile,
    substitutions: Vec<Substitution>,
    /// The stored chains the queries read.
    chains: Vec<NodeId>,
}

impl Translator<'_> {
    fn node(&self, id: NodeId) -> &PlanNode {
        &self.plan.nodes[id]
    }

    fn unsupported(&self, node: NodeId, reason: impl Into<String>) -> TranslationError {
        TranslationError::UnsupportedShape {
            node,
            reason: reason.into(),
        }
    }

    fn root_query(&mut self, root: NodeId) -> Result<String, TranslationError> {
        match self.relation(root)? {
            Relation::Query { sql, .. } => Ok(sql),
            Relation::Raw { .. } => Err(self.unsupported(root, "raw samples as a query result")),
        }
    }

    fn relation(&mut self, id: NodeId) -> Result<Relation, TranslationError> {
        let plan = self.plan;
        match &plan.nodes[id].operator {
            PlanOperator::TimeRange {
                child, range_ms, ..
            } => self.raw(id, *child, *range_ms),
            PlanOperator::Finalize { child } => self.finalized(id, *child),
            PlanOperator::Aggregate {
                child,
                measures,
                reduction,
                ..
            } => self.aggregate(id, *child, measures, reduction),
            _ => Err(self.unsupported(id, "this operator here")),
        }
    }

    /// `TimeRange` over a query-time time-series Scan.
    fn raw(&self, id: NodeId, scan: NodeId, range_ms: u64) -> Result<Relation, TranslationError> {
        let scan_node = self.node(scan);
        let PlanOperator::Scan {
            source: Source::TimeSeries { metric },
            predicates,
        } = &scan_node.operator
        else {
            return Err(self.unsupported(id, "time range over this operator"));
        };
        if scan_node.timing != Timing::QueryTime {
            return Err(self.unsupported(id, "time range over an ingestion-time scan"));
        }
        let mut conditions: Vec<String> = predicates
            .iter()
            .map(|predicate| match predicate {
                Predicate::LabelEquals { column, value } => Ok(format!(
                    "{} = {}",
                    quoted(&scan_node.output_columns[*column]),
                    string_literal(value)
                )),
                Predicate::Other(_) => Err(self.unsupported(scan, "predicate")),
            })
            .collect::<Result<_, _>>()?;
        conditions.push(format!(
            "ts > fromUnixTimestamp64Milli({{t:Int64}} - {range_ms}) \
             AND ts <= fromUnixTimestamp64Milli({{t:Int64}})"
        ));
        Ok(Relation::Raw {
            table: source_table(metric),
            labels: labels_of(&self.node(id).output_columns),
            conditions: conditions.join(" AND "),
        })
    }

    /// `FinalizeExactAccumulator` over a stored chain's merged windows or over
    /// a query-time `SummaryAgg`.
    fn finalized(&mut self, id: NodeId, child: NodeId) -> Result<Relation, TranslationError> {
        match self.node(child).operator.clone() {
            PlanOperator::SummaryMerge { children, .. } => {
                let [range] = children[..] else {
                    return Err(self.unsupported(child, "merge of several inputs"));
                };
                let PlanOperator::TimeRange {
                    child: chain,
                    range_ms,
                    ..
                } = self.node(range).operator
                else {
                    return Err(self.unsupported(child, "merge of this operator"));
                };
                self.stored_read(chain, range_ms)
            }
            PlanOperator::SummaryAgg {
                child: range,
                family: SummaryFamily::Exact(kind),
                reduction: Reduction::PerEntity,
                ..
            } => {
                let PlanOperator::TimeRange {
                    child: scan,
                    range_ms,
                    ..
                } = self.node(range).operator
                else {
                    return Err(self.unsupported(child, "summary over this operator"));
                };
                let Relation::Raw {
                    table,
                    labels,
                    conditions,
                } = self.raw(range, scan, range_ms)?
                else {
                    unreachable!("raw() returns Raw");
                };
                let group = column_list(&labels);
                if let Some(is_rate) = counter_kind(&kind) {
                    let inner = format!(
                        "SELECT {group}, \
                         arraySort(groupArray((toUnixTimestamp64Milli(ts), value))) AS points, \
                         arrayMap(p -> p.2, points) AS vals, \
                         length(points) AS n, \
                         points[1].1 AS first_ms, \
                         points[n].1 AS last_ms, \
                         vals[1] AS v_first, \
                         vals[n] - vals[1] + arraySum(arrayMap((cur, prev) -> if(cur < prev, prev, 0), \
                         arrayPopFront(vals), arrayPopBack(vals))) AS increase \
                         FROM {table} WHERE {conditions} GROUP BY {group} HAVING n >= 2"
                    );
                    return Ok(Relation::Query {
                        sql: extrapolated(&group, &inner, range_ms, is_rate),
                        labels,
                    });
                }
                let aggregate = plain_kind(&kind)
                    .ok_or_else(|| self.unsupported(child, format!("exact {kind}")))?;
                Ok(Relation::Query {
                    sql: format!(
                        "SELECT {group}, {} AS value FROM {table} WHERE {conditions} GROUP BY {group}",
                        of_value(aggregate)
                    ),
                    labels,
                })
            }
            _ => Err(self.unsupported(id, "finalize over this operator")),
        }
    }

    /// The merged windows of stored chain `chain` in `(t − range_ms, t]`.
    fn stored_read(&mut self, chain: NodeId, range_ms: u64) -> Result<Relation, TranslationError> {
        let (kind, labels) = self.chain_shape(chain)?;
        self.chains.push(chain);
        let group = column_list(&labels);
        let window =
            format!("window_end_ms > {{t:Int64}} - {range_ms} AND window_end_ms <= {{t:Int64}}");
        if let Some(is_rate) = counter_kind(&kind) {
            self.substitutions.push(Substitution {
                node: chain,
                rule: "clickhouse.stored_rate.delta_sum_timestamp".into(),
                guarantee_difference: "a counter reset adds 0 for the step across it, where \
                                       PromQL adds the first value after the reset"
                    .into(),
            });
            let inner = format!(
                "SELECT {group}, countMerge(s_count) AS n, minMerge(s_first_ms) AS first_ms, \
                 maxMerge(s_last_ms) AS last_ms, argMinMerge(s_first_value) AS v_first, \
                 deltaSumTimestampMerge(s_increase) AS increase \
                 FROM v{chain} WHERE {window} GROUP BY {group} HAVING n >= 2"
            );
            return Ok(Relation::Query {
                sql: extrapolated(&group, &inner, range_ms, is_rate),
                labels,
            });
        }
        let aggregate = plain_kind(&kind).expect("checked by chain_shape");
        Ok(Relation::Query {
            sql: format!(
                "SELECT {group}, {aggregate}Merge(state) AS value FROM v{chain} WHERE {window} GROUP BY {group}"
            ),
            labels,
        })
    }

    /// The exact kind and label columns of a stored chain this translator
    /// supports: ingestion-time, kept across evaluations, per series, over an
    /// ingestion-time time-series Scan.
    fn chain_shape(&self, chain: NodeId) -> Result<(String, Vec<String>), TranslationError> {
        let node = self.node(chain);
        let PlanOperator::WindowChain {
            child,
            family: SummaryFamily::Exact(kind),
            reduction,
            ..
        } = &node.operator
        else {
            return Err(self.unsupported(chain, "time range over this operator"));
        };
        if *reduction != Reduction::PerEntity {
            return Err(self.unsupported(chain, "grouped window chain"));
        }
        if node.timing != Timing::IngestionTime || node.lifetime != Lifetime::AcrossEvaluations {
            return Err(self.unsupported(chain, "window chain not stored at ingestion"));
        }
        let scan = self.node(*child);
        if !matches!(
            scan.operator,
            PlanOperator::Scan {
                source: Source::TimeSeries { .. },
                ..
            }
        ) || scan.timing != Timing::IngestionTime
        {
            return Err(self.unsupported(chain, "window chain over this operator"));
        }
        Ok((kind.clone(), labels_of(&node.output_columns)))
    }

    /// The table and materialized view of stored chain `chain`.
    fn chain_ddl(&self, chain: NodeId) -> Result<(String, String), TranslationError> {
        let (kind, labels) = self.chain_shape(chain)?;
        let PlanOperator::WindowChain {
            child, width_ms, ..
        } = self.node(chain).operator
        else {
            unreachable!("checked by chain_shape");
        };
        let PlanOperator::Scan {
            source: Source::TimeSeries { metric },
            ..
        } = &self.node(child).operator
        else {
            unreachable!("checked by chain_shape");
        };
        let group = column_list(&labels);
        let label_columns: String = labels
            .iter()
            .map(|label| format!("{} String, ", quoted(label)))
            .collect();
        let ts_ms = "toUnixTimestamp64Milli(ts)";
        let (state_columns, state_values) = if counter_kind(&kind).is_some() {
            (
                "s_count AggregateFunction(count), s_first_ms AggregateFunction(min, Int64), \
                 s_last_ms AggregateFunction(max, Int64), \
                 s_first_value AggregateFunction(argMin, Float64, Int64), \
                 s_increase AggregateFunction(deltaSumTimestamp, Float64, Int64)"
                    .to_string(),
                format!(
                    "countState() AS s_count, minState({ts_ms}) AS s_first_ms, \
                     maxState({ts_ms}) AS s_last_ms, argMinState(value, {ts_ms}) AS s_first_value, \
                     deltaSumTimestampState(value, {ts_ms}) AS s_increase"
                ),
            )
        } else {
            let aggregate = plain_kind(&kind).expect("checked by chain_shape");
            let state_type = if aggregate == "count" {
                "AggregateFunction(count)".to_string()
            } else {
                format!("AggregateFunction({aggregate}, Float64)")
            };
            let state = if aggregate == "count" {
                "countState()".to_string()
            } else {
                format!("{aggregate}State(value)")
            };
            (format!("state {state_type}"), format!("{state} AS state"))
        };
        let table = format!(
            "CREATE TABLE v{chain} ({label_columns}window_end_ms Int64, {state_columns}) \
             ENGINE = AggregatingMergeTree ORDER BY (window_end_ms, {group})"
        );
        let view = format!(
            "CREATE MATERIALIZED VIEW v{chain}_mv TO v{chain} AS SELECT {group}, \
             (intDiv({ts_ms} - 1, {width_ms}) + 1) * {width_ms} AS window_end_ms, {state_values} \
             FROM {} GROUP BY {group}, window_end_ms",
            source_table(metric)
        );
        Ok((table, view))
    }

    fn aggregate(
        &mut self,
        id: NodeId,
        child: NodeId,
        measures: &[Measure],
        reduction: &Reduction,
    ) -> Result<Relation, TranslationError> {
        let [measure] = measures else {
            return Err(self.unsupported(id, "several measures"));
        };
        let value = match (measure.kind.as_str(), measure.col) {
            ("sum" | "min" | "max" | "avg", None) => format!("{}(value)", measure.kind),
            ("count", None) => "count()".to_string(),
            ("quantile", None) => {
                let q = measure
                    .arguments
                    .get("q")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| self.unsupported(id, "quantile without q"))?;
                format!("quantileExactInclusive({q})(value)")
            }
            (kind, _) => return Err(self.unsupported(id, format!("measure {kind}"))),
        };
        let input = self.relation(child)?;
        let input_labels = match &input {
            Relation::Raw { labels, .. } | Relation::Query { labels, .. } => labels.clone(),
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
                            .cloned()
                            .ok_or_else(|| self.unsupported(id, "group column"))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
        };
        let from = match input {
            Relation::Raw {
                table, conditions, ..
            } => format!("{table} WHERE {conditions}"),
            Relation::Query { sql, .. } => format!("( {sql})"),
        };
        let sql = if labels.is_empty() {
            format!("SELECT {value} AS value FROM {from} HAVING count() > 0")
        } else {
            let group = column_list(&labels);
            format!("SELECT {group}, {value} AS value FROM {from} GROUP BY {group}")
        };
        Ok(Relation::Query { sql, labels })
    }
}

/// Prometheus `extrapolatedRate` (`promql/functions.go`, v3.15.1) over
/// `inner`, which gives per series `n`, `first_ms`, `last_ms`, `v_first` and
/// the reset-corrected `increase` of the samples in `(t − range_ms, t]`
/// (at least 2): the 1.1 × average-gap threshold first, then the zero-point
/// clamp; divided by the range in seconds for a rate.
fn extrapolated(group: &str, inner: &str, range_ms: u64, is_rate: bool) -> String {
    let per_second = if is_rate {
        format!(" / {}", seconds(range_ms))
    } else {
        String::new()
    };
    format!(
        "SELECT {group}, increase * (sampled + least(d_start, d_zero) + d_end) / sampled{per_second} AS value \
         FROM ( SELECT {group}, increase, (last_ms - first_ms) / 1000 AS sampled, \
         sampled / (n - 1) AS avg_gap, \
         if((first_ms - ({{t:Int64}} - {range_ms})) / 1000 >= 1.1 * avg_gap, avg_gap / 2, \
         (first_ms - ({{t:Int64}} - {range_ms})) / 1000) AS d_start, \
         if(increase > 0 AND v_first >= 0, sampled * v_first / increase, d_start) AS d_zero, \
         if(({{t:Int64}} - last_ms) / 1000 >= 1.1 * avg_gap, avg_gap / 2, ({{t:Int64}} - last_ms) / 1000) AS d_end \
         FROM ( {inner}))"
    )
}

/// `300000` ms as `300`; a fraction of a second keeps its decimals.
fn seconds(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        (ms / 1000).to_string()
    } else {
        format!("{}", ms as f64 / 1000.0)
    }
}

fn of_value(aggregate: &str) -> String {
    if aggregate == "count" {
        "count()".into()
    } else {
        format!("{aggregate}(value)")
    }
}

/// A node's label columns: its output columns other than time, value and state.
fn labels_of(columns: &[String]) -> Vec<String> {
    columns
        .iter()
        .filter(|c| !matches!(c.as_str(), "ts" | "value" | "window_end" | "state"))
        .cloned()
        .collect()
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
        assert_eq!(seconds(300_000), "300");
        assert_eq!(seconds(1_500), "1.5");
    }

    #[test]
    fn identifiers_and_literals_are_escaped() {
        assert_eq!(quoted("a`b"), "`a\\`b`");
        assert_eq!(string_literal("it's"), "'it\\'s'");
    }
}

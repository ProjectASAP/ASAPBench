//! An ASAPPlanner plan file (`InstallablePlan`, wire version 15), read with
//! what the translators use.
//!
//! The decoder works on JSON values rather than mirroring the planner's
//! types: sketch-bench does not depend on ASAPPlanner, and a field the
//! translators do not read is ignored. Every field that can change an
//! answer (predicates, filters, HAVING, the summary input, a regrouping
//! merge, `without` grouping) is decoded, so a translator either has a rule
//! for it or refuses the node.

use serde_json::{Map, Value};

use crate::{NodeId, TranslationError};

/// The `InstallablePlan` wire version this decoder reads (ASAPPlanner
/// `INSTALLABLE_PLAN_WIRE_VERSION`).
pub const SUPPORTED_PLAN_WIRE_VERSION: u32 = 15;

/// A plan file: the planner's selected DAG and one query text per root.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanFile {
    /// In id order: `nodes[i].id == i`.
    pub nodes: Vec<PlanNode>,
    pub edges: Vec<PlanEdge>,
    /// One root per query, in workload order.
    pub roots: Vec<NodeId>,
    pub query_texts: Vec<String>,
}

/// One edge of the DAG: `consumer` reads `producer`'s output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanEdge {
    pub producer: NodeId,
    pub consumer: NodeId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    pub id: NodeId,
    pub operator: PlanOperator,
    /// When the node runs (`output_state.timing` on the wire).
    pub timing: Timing,
    /// How long its output is kept.
    pub lifetime: Lifetime,
    /// The names of the node's output columns, in order.
    pub output_columns: Vec<String>,
    /// The output columns that are labels (wire dtype `{"Plain": "utf8"}`),
    /// in order, including the series identity.
    pub label_columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    IngestionTime,
    QueryTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    None,
    OneEvaluation,
    AcrossEvaluations,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanOperator {
    Scan {
        source: Source,
        predicates: Vec<Predicate>,
    },
    TimeRange {
        child: NodeId,
        kind: RangeKind,
        range_ms: u64,
    },
    /// `SummaryTumblingWindowChain`: per-window state built at `width_ms`.
    WindowChain {
        child: NodeId,
        family: SummaryFamily,
        width_ms: u64,
        reduction: Reduction,
        input: Value,
        filter: Option<Value>,
    },
    SummaryAgg {
        child: NodeId,
        family: SummaryFamily,
        reduction: Reduction,
        input: Value,
        filter: Option<Value>,
    },
    SummaryMerge {
        children: Vec<NodeId>,
        group_by: Option<Vec<usize>>,
    },
    /// `FinalizeExactAccumulator`.
    Finalize {
        child: NodeId,
    },
    SummaryEstimate {
        summary_input: NodeId,
    },
    Aggregate {
        child: NodeId,
        measures: Vec<Measure>,
        reduction: Reduction,
        filters: Vec<Value>,
        having: Option<Value>,
    },
    /// Any operator this decoder does not read, named `<side>.<operator>`,
    /// e.g. `"ASAP.FancyNewOp"`. Decoded so the plan loads; never translated.
    Other {
        kind: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    TimeSeries {
        metric: String,
    },
    /// A SQL table; no translator reads one yet.
    Table,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeKind {
    Range,
    Instant,
}

/// A Scan predicate. `{"Compare": {"left": {"Column": 2}, "op": "Eq",
/// "right": {"Literal": {"Utf8": "j3"}}, ...}}` is `LabelEquals`; every
/// other predicate is kept as is.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    LabelEquals { column: usize, value: String },
    Other(Value),
}

/// `"PerEntity"`, `{"Reduce": [2]}` or `{"Reduce": {"without": [2]}}`.
/// Columns are indices into the child's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reduction {
    PerEntity,
    Reduce { columns: Vec<usize>, without: bool },
}

/// One measure of an `Aggregate`: `kind` as ASAPPlanner's `AggIntent` JSON
/// (`"sum"`, `"quantile"`, `"i_rate"`), the input `column` (wire `col`),
/// and its other fields (e.g. `q`).
#[derive(Debug, Clone, PartialEq)]
pub struct Measure {
    pub kind: String,
    pub column: Option<usize>,
    pub arguments: Map<String, Value>,
}

/// ASAPPlanner `SummaryFamily`: a sketch algorithm or an exact kind, read
/// from a node's `summary_type` (`{"ExactAggregate": ["Rate", "Rate"]}` or
/// `{"Sketch": "Kll"}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryFamily {
    Sketch(String),
    Exact(String),
}

impl PlanFile {
    /// Reads a plan file. Its `wire_version` is checked before anything else.
    pub fn from_json(text: &str) -> Result<PlanFile, TranslationError> {
        let value: Value = serde_json::from_str(text).map_err(|e| malformed(e.to_string()))?;
        Self::from_value(value)
    }

    pub fn from_value(value: Value) -> Result<PlanFile, TranslationError> {
        let found = value
            .get("wire_version")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());
        if found != Some(SUPPORTED_PLAN_WIRE_VERSION) {
            return Err(TranslationError::UnsupportedWireVersion {
                found,
                supported: SUPPORTED_PLAN_WIRE_VERSION,
            });
        }
        let dag = field(&value, "dag")?;
        if let Some(root) = array(dag, "scalar_roots")?.first() {
            let query_index = root
                .get(0)
                .and_then(Value::as_u64)
                .ok_or_else(|| malformed("scalar root"))?;
            return Err(TranslationError::UnsupportedScalarQuery {
                query_index: query_index as usize,
            });
        }
        let nodes = array(dag, "nodes")?
            .iter()
            .enumerate()
            .map(|(index, node)| node_of(index, node))
            .collect::<Result<Vec<_>, _>>()?;
        let edges = array(dag, "edges")?
            .iter()
            .map(|edge| {
                Ok(PlanEdge {
                    producer: index(field(edge, "producer")?)?,
                    consumer: index(field(edge, "consumer")?)?,
                })
            })
            .collect::<Result<Vec<_>, TranslationError>>()?;
        let roots = array(dag, "roots")?
            .iter()
            .map(index)
            .collect::<Result<Vec<_>, _>>()?;
        let query_texts = array(&value, "query_texts")?
            .iter()
            .map(|text| {
                text.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| malformed("query text"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if roots.len() != query_texts.len() {
            return Err(malformed(format!(
                "{} roots but {} query texts",
                roots.len(),
                query_texts.len()
            )));
        }
        let plan = PlanFile {
            nodes,
            edges,
            roots,
            query_texts,
        };
        plan.check_ids()?;
        Ok(plan)
    }

    /// Every id the file names is a node, and every node reads only nodes
    /// with smaller ids (so the DAG has no cycle): translators can then
    /// index nodes by id and recurse over children.
    fn check_ids(&self) -> Result<(), TranslationError> {
        let count = self.nodes.len();
        let in_range = |id: NodeId, what: &str| {
            if id < count {
                Ok(())
            } else {
                Err(malformed(format!("{what} {id} is not a node")))
            }
        };
        for &root in &self.roots {
            in_range(root, "root")?;
        }
        for edge in &self.edges {
            in_range(edge.producer, "edge producer")?;
            in_range(edge.consumer, "edge consumer")?;
        }
        for node in &self.nodes {
            for child in node.operator.inputs() {
                if child >= node.id {
                    return Err(malformed(format!(
                        "node {} reads node {child}, which does not come before it",
                        node.id
                    )));
                }
            }
        }
        Ok(())
    }

    /// The nodes that read `node`'s output, in edge order.
    pub fn readers_of(&self, node: NodeId) -> Vec<NodeId> {
        self.edges
            .iter()
            .filter(|edge| edge.producer == node)
            .map(|edge| edge.consumer)
            .collect()
    }
}

impl PlanOperator {
    /// The nodes this operator reads.
    pub fn inputs(&self) -> Vec<NodeId> {
        match self {
            PlanOperator::Scan { .. } | PlanOperator::Other { .. } => Vec::new(),
            PlanOperator::TimeRange { child, .. }
            | PlanOperator::WindowChain { child, .. }
            | PlanOperator::SummaryAgg { child, .. }
            | PlanOperator::Finalize { child }
            | PlanOperator::Aggregate { child, .. } => vec![*child],
            PlanOperator::SummaryEstimate { summary_input } => vec![*summary_input],
            PlanOperator::SummaryMerge { children, .. } => children.clone(),
        }
    }
}

fn node_of(position: usize, node: &Value) -> Result<PlanNode, TranslationError> {
    let id = index(field(node, "id")?)?;
    if id != position {
        return Err(malformed(format!("node {id} at position {position}")));
    }
    let timing = match field(field(node, "output_state")?, "timing")?.as_str() {
        Some("ingestion_time") => Timing::IngestionTime,
        Some("query_time") => Timing::QueryTime,
        other => return Err(malformed(format!("node {id}: timing {other:?}"))),
    };
    let lifetime = match field(node, "lifetime")? {
        Value::String(s) if s == "None" => Lifetime::None,
        Value::Object(map) if map.contains_key("OneEvaluation") => Lifetime::OneEvaluation,
        Value::Object(map) if map.contains_key("AcrossEvaluations") => Lifetime::AcrossEvaluations,
        other => return Err(malformed(format!("node {id}: lifetime {other}"))),
    };
    let columns = match node.get("output_schema") {
        Some(schema) => columns_of(schema)?,
        None => Vec::new(),
    };
    let output_columns = columns.iter().map(|(name, _)| name.clone()).collect();
    let label_columns = columns
        .into_iter()
        .filter(|(_, is_label)| *is_label)
        .map(|(name, _)| name)
        .collect();
    Ok(PlanNode {
        id,
        operator: operator_of(field(node, "payload")?)?,
        timing,
        lifetime,
        output_columns,
        label_columns,
    })
}

fn operator_of(payload: &Value) -> Result<PlanOperator, TranslationError> {
    let (side, body) = single_entry(payload, "payload")?;
    let (name, body) = single_entry(body, "operator")?;
    let operator = match name.as_str() {
        "Scan" => PlanOperator::Scan {
            source: source_of(field(body, "source")?)?,
            predicates: array(body, "predicates")?
                .iter()
                .map(predicate_of)
                .collect(),
        },
        "TimeRange" => {
            let range = field(body, "range")?;
            let secs = field(range, "secs")?
                .as_u64()
                .ok_or_else(|| malformed("secs"))?;
            let nanos = field(range, "nanos")?
                .as_u64()
                .ok_or_else(|| malformed("nanos"))?;
            if nanos % 1_000_000 != 0 {
                return Err(malformed("a range finer than a millisecond"));
            }
            PlanOperator::TimeRange {
                child: index(field(body, "child")?)?,
                kind: match field(body, "kind")?.as_str() {
                    Some("range") => RangeKind::Range,
                    Some("instant") => RangeKind::Instant,
                    other => return Err(malformed(format!("range kind {other:?}"))),
                },
                range_ms: secs
                    .checked_mul(1000)
                    .and_then(|ms| ms.checked_add(nanos / 1_000_000))
                    .ok_or_else(|| malformed("range too long"))?,
            }
        }
        "SummaryTumblingWindowChain" => PlanOperator::WindowChain {
            child: index(field(body, "child")?)?,
            family: family_of(field(body, "summary_type")?)?,
            width_ms: field(body, "tumbling_window_width_ms")?
                .as_u64()
                .ok_or_else(|| malformed("window width"))?,
            reduction: reduction_of(field(body, "reduction")?)?,
            input: field(body, "input")?.clone(),
            filter: optional(body, "filter"),
        },
        "SummaryAgg" => PlanOperator::SummaryAgg {
            child: index(field(body, "child")?)?,
            family: family_of(field(body, "summary_type")?)?,
            reduction: reduction_of(field(body, "reduction")?)?,
            input: field(body, "input")?.clone(),
            filter: optional(body, "filter"),
        },
        "SummaryMerge" => PlanOperator::SummaryMerge {
            children: array(body, "children")?
                .iter()
                .map(index)
                .collect::<Result<Vec<_>, _>>()?,
            group_by: match optional(body, "group_by") {
                None => None,
                Some(columns) => Some(
                    columns
                        .as_array()
                        .ok_or_else(|| malformed("group_by"))?
                        .iter()
                        .map(index)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            },
        },
        "FinalizeExactAccumulator" => PlanOperator::Finalize {
            child: index(field(body, "child")?)?,
        },
        "SummaryEstimate" => PlanOperator::SummaryEstimate {
            summary_input: index(field(body, "summary_input")?)?,
        },
        "Aggregate" => PlanOperator::Aggregate {
            child: index(field(body, "child")?)?,
            measures: array(body, "measures")?
                .iter()
                .map(measure_of)
                .collect::<Result<Vec<_>, _>>()?,
            reduction: reduction_of(field(body, "reduction")?)?,
            filters: body
                .get("filters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            having: optional(body, "having"),
        },
        _ => PlanOperator::Other {
            kind: format!("{side}.{name}"),
        },
    };
    Ok(operator)
}

fn source_of(source: &Value) -> Result<Source, TranslationError> {
    let (kind, body) = single_entry(source, "source")?;
    match kind.as_str() {
        "TimeSeries" => Ok(Source::TimeSeries {
            metric: field(body, "metric")?
                .as_str()
                .ok_or_else(|| malformed("metric"))?
                .to_string(),
        }),
        "Table" => Ok(Source::Table),
        other => Err(malformed(format!("source {other}"))),
    }
}

fn predicate_of(predicate: &Value) -> Predicate {
    let label_equals = || {
        let compare = predicate.get("Compare")?;
        if compare.get("op")?.as_str()? != "Eq" {
            return None;
        }
        let column = compare.get("left")?.get("Column")?.as_u64()? as usize;
        let value = compare
            .get("right")?
            .get("Literal")?
            .get("Utf8")?
            .as_str()?;
        Some(Predicate::LabelEquals {
            column,
            value: value.to_string(),
        })
    };
    label_equals().unwrap_or_else(|| Predicate::Other(predicate.clone()))
}

fn family_of(summary_type: &Value) -> Result<SummaryFamily, TranslationError> {
    if let Some(exact) = summary_type.get("ExactAggregate") {
        let kind = exact
            .get(0)
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("exact kind"))?;
        return Ok(SummaryFamily::Exact(kind.to_string()));
    }
    if let Some(sketch) = summary_type.get("Sketch").and_then(Value::as_str) {
        return Ok(SummaryFamily::Sketch(sketch.to_string()));
    }
    Err(malformed(format!("summary type {summary_type}")))
}

fn reduction_of(reduction: &Value) -> Result<Reduction, TranslationError> {
    if reduction.as_str() == Some("PerEntity") {
        return Ok(Reduction::PerEntity);
    }
    let reduce = field(reduction, "Reduce")?;
    let (columns, without) = match reduce.get("without") {
        Some(columns) => (columns, true),
        None => (reduce, false),
    };
    Ok(Reduction::Reduce {
        columns: columns
            .as_array()
            .ok_or_else(|| malformed("reduce columns"))?
            .iter()
            .map(index)
            .collect::<Result<Vec<_>, _>>()?,
        without,
    })
}

fn measure_of(measure: &Value) -> Result<Measure, TranslationError> {
    let mut arguments = measure
        .as_object()
        .ok_or_else(|| malformed("measure"))?
        .clone();
    let kind = arguments
        .remove("kind")
        .and_then(|kind| kind.as_str().map(str::to_string))
        .ok_or_else(|| malformed("measure kind"))?;
    let column = match arguments.remove("col") {
        None | Some(Value::Null) => None,
        Some(col) => Some(index(&col)?),
    };
    Ok(Measure {
        kind,
        column,
        arguments,
    })
}

/// Each output column's name, and whether it is a label (`{"Plain": "utf8"}`).
fn columns_of(schema: &Value) -> Result<Vec<(String, bool)>, TranslationError> {
    array(schema, "fields")?
        .iter()
        .map(|column| {
            let name = field(column, "name")?
                .as_str()
                .ok_or_else(|| malformed("column name"))?;
            let is_label = column.get("dtype") == Some(&serde_json::json!({"Plain": "utf8"}));
            Ok((name.to_string(), is_label))
        })
        .collect()
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, TranslationError> {
    value
        .get(name)
        .ok_or_else(|| malformed(format!("missing `{name}`")))
}

fn optional(value: &Value, name: &str) -> Option<Value> {
    value.get(name).filter(|v| !v.is_null()).cloned()
}

fn array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>, TranslationError> {
    field(value, name)?
        .as_array()
        .ok_or_else(|| malformed(format!("`{name}` is not a list")))
}

fn index(value: &Value) -> Result<usize, TranslationError> {
    value
        .as_u64()
        .map(|v| v as usize)
        .ok_or_else(|| malformed(format!("{value} is not an index")))
}

fn single_entry<'a>(
    value: &'a Value,
    what: &str,
) -> Result<(&'a String, &'a Value), TranslationError> {
    let map = value
        .as_object()
        .filter(|map| map.len() == 1)
        .ok_or_else(|| malformed(format!("{what} {value}")))?;
    Ok(map.iter().next().expect("one entry"))
}

fn malformed(reason: impl Into<String>) -> TranslationError {
    TranslationError::MalformedPlan {
        reason: reason.into(),
    }
}

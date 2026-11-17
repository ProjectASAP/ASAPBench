//! A deterministic in-process engine on a shared fake clock, for the
//! harness's own tests.
//!
//! It answers `sum_over_time(<metric>[<w>])` and `count_over_time(<metric>[<w>])`
//! per series over `(t - w, t]`. Ingesting a chunk charges `ns_per_call +
//! ns_per_sample` per sample to the clock; evaluating charges `ns_per_call +
//! ns_per_series` per answered series. Example: with the defaults (50, 150
//! and 10,000 ns), ingesting 1,000 samples costs 60,000 ns and evaluating
//! 4 series 10,600 ns.
//!
//! With a [`FakeServer`] it plays an external server instead: its work is
//! charged to the server thread's `schedstat` under a fake `/proc` root, not
//! to the harness's clock, and it reports `CpuAccounting::Processes`.
//!
//! Its plan has one `scan` node, shared by every query and charged
//! `ns_per_call`, and one node per query, named after the query id and
//! charged per answered series; the operator timing hook times each node
//! once per evaluation.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::harness::cgroup::CgroupSource;
use crate::harness::data::{SampleChunk, SeriesLabels};
use crate::harness::engine::{
    Answers, ArmSpec, Calibratable, CostTarget, CpuAccounting, EngineDescription, EngineError,
    EngineUnderTest, Footprint, InstalledArm, NodeProfile, PlanSummary, QueryLanguage, StepReport,
    WorkloadSpec,
};
use crate::harness::protocol::{CpuClock, ProcessCpuFiles};

/// The ticks of a [`FakeClock`], in ns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FakeTicks {
    pub thread_ns: u64,
    pub process_ns: u64,
    pub wall_ns: u64,
}

/// A clock only the fake engine (or a test) advances. Clones share the ticks.
#[derive(Debug, Clone, Default)]
pub struct FakeClock(pub Rc<Cell<FakeTicks>>);

impl FakeClock {
    /// Work on the calling thread: advances thread, process and wall time by `ns`.
    pub fn charge_thread(&self, ns: u64) {
        let ticks = self.0.get();
        self.0.set(FakeTicks {
            thread_ns: ticks.thread_ns + ns,
            process_ns: ticks.process_ns + ns,
            wall_ns: ticks.wall_ns + ns,
        });
    }

    /// Work on another thread of this process: advances process and wall
    /// time by `ns`, thread time not at all.
    pub fn charge_other_thread(&self, ns: u64) {
        let ticks = self.0.get();
        self.0.set(FakeTicks {
            process_ns: ticks.process_ns + ns,
            wall_ns: ticks.wall_ns + ns,
            ..ticks
        });
    }
}

impl CpuClock for FakeClock {
    fn thread_cpu_ns(&self) -> u64 {
        self.0.get().thread_ns
    }
    fn process_cpu_ns(&self) -> u64 {
        self.0.get().process_ns
    }
    fn wall_ns(&self) -> u64 {
        self.0.get().wall_ns
    }
}

/// A fake external server: one pid under a fake `/proc` root, with a
/// `<root>/<pid>/stat` in state `S` and one thread whose
/// `<root>/<pid>/task/<pid>/schedstat` the fake rewrites after every charge,
/// its first field the server's CPU in ns. No cgroup files, so the clock
/// reads `schedstat`. Clones share the server's CPU.
/// Example: after 10,150 ns of work, its `schedstat` reads 10150.
#[derive(Debug, Clone)]
pub struct FakeServer {
    pub pid: u32,
    /// The fake `/proc` (and an empty cgroup mount beside it).
    pub cpu_files: ProcessCpuFiles,
    cpu_ns: Rc<Cell<u64>>,
    /// Set by [`FakeServer::start_in_cgroup`]: the group directory whose
    /// `cpu.stat` the fake rewrites after every charge.
    pub cgroup_dir: Option<PathBuf>,
}

impl FakeServer {
    /// Creates `<root>/<pid>/stat` and the thread's `schedstat` with no CPU used.
    pub fn start(root: &Path, pid: u32) -> std::io::Result<Self> {
        let process = root.join(pid.to_string());
        std::fs::create_dir_all(process.join("task").join(pid.to_string()))?;
        std::fs::write(
            process.join("stat"),
            format!("{pid} (fake server) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0\n"),
        )?;
        let server = FakeServer {
            pid,
            cpu_files: ProcessCpuFiles {
                proc_root: root.to_path_buf(),
                cgroup_root: root.join("cgroup"),
            },
            cpu_ns: Rc::default(),
            cgroup_dir: None,
        };
        server.write_schedstat()?;
        Ok(server)
    }

    /// A server in its own cgroup v2 group at `cgroup_dir`: after every
    /// charge its `cpu.stat` reads `usage_usec` = CPU in whole µs (rounded
    /// down, as the kernel reports it) and `nr_throttled 0`; the memory files
    /// read `memory.current` 3 MiB, `memory.peak` 5 MiB, `memory.stat`
    /// `anon` 2 MiB and `file` 1 MiB. The engine then reports
    /// `CpuAccounting::Cgroup(Local(cgroup_dir))`.
    pub fn start_in_cgroup(root: &Path, pid: u32, cgroup_dir: &Path) -> std::io::Result<Self> {
        let mut server = FakeServer::start(root, pid)?;
        std::fs::create_dir_all(cgroup_dir)?;
        std::fs::write(cgroup_dir.join("memory.current"), "3145728\n")?;
        std::fs::write(cgroup_dir.join("memory.peak"), "5242880\n")?;
        std::fs::write(
            cgroup_dir.join("memory.stat"),
            "anon 2097152\nfile 1048576\nkernel 4096\n",
        )?;
        server.cgroup_dir = Some(cgroup_dir.to_path_buf());
        server.write_schedstat()?;
        Ok(server)
    }

    /// The server's CPU so far, in ns.
    pub fn cpu_ns(&self) -> u64 {
        self.cpu_ns.get()
    }

    /// Charges `ns` of server CPU, as work between steps does (such as a
    /// background merge).
    pub fn charge(&self, ns: u64) {
        self.cpu_ns.set(self.cpu_ns.get() + ns);
        self.write_schedstat()
            .unwrap_or_else(|error| panic!("fake server {} schedstat: {error}", self.pid));
    }

    /// The thread's `schedstat`: CPU ns, wait ns, time slices. The CPU is
    /// zero-padded to 20 digits so every write has the same length and
    /// overwrites the file in place: truncating and rewriting a file makes
    /// some file systems (ext4) flush it on close, about 100 ms per write.
    fn write_schedstat(&self) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(dir) = &self.cgroup_dir {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join("cpu.stat"))?
                .write_all(
                    format!(
                        "usage_usec {:020}\nnr_periods 0\nnr_throttled 0\nthrottled_usec 0\n",
                        self.cpu_ns.get() / 1_000
                    )
                    .as_bytes(),
                )?;
        }
        let path = self
            .cpu_files
            .proc_root
            .join(self.pid.to_string())
            .join("task")
            .join(self.pid.to_string())
            .join("schedstat");
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?
            .write_all(format!("{:020} 0 0\n", self.cpu_ns.get()).as_bytes())
    }
}

/// A broken behavior the conformance kit must catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeFault {
    /// Adds 0.5 to every answer from the second install on.
    NondeterministicAnswers,
    /// Counts a sample at the window start, `[t - w, t]`.
    CountsWindowStart,
    /// `charged_units` reports twice the units the calibration closure returns.
    CalibrationUnitsDouble,
    /// Per ingest, charges 1 ms on the calling thread and 8 ms on another
    /// thread: 9 ms of process CPU, 1 ms of it on the calling thread.
    WorksOnAnotherThread,
}

/// The fake engine.
#[derive(Debug, Clone)]
pub struct FakeEngine {
    pub clock: FakeClock,
    pub ns_per_sample: u64,
    pub ns_per_series: u64,
    pub ns_per_call: u64,
    /// Charged at every install, outside every step (0 by default), as a
    /// server's setup work.
    pub ns_per_install: u64,
    /// Reported in every `StepReport`, like an external engine's own counter.
    pub engine_reported_cpu_ns: Option<u64>,
    pub cpu_accounting: CpuAccounting,
    pub faults: Vec<FakeFault>,
    /// Arms installed so far; [`FakeFault::NondeterministicAnswers`] reads it.
    pub installs: Cell<u32>,
    /// When set, the fake is an external server: work is charged to it, not
    /// to `clock`, and `cpu_accounting()` is `Processes([server.pid])`.
    pub server: Option<FakeServer>,
}

impl Default for FakeEngine {
    /// 50 ns per sample, 150 ns per series, 10 µs per call, on a new clock,
    /// `CallingThread`, no faults.
    fn default() -> Self {
        FakeEngine {
            clock: FakeClock::default(),
            ns_per_sample: 50,
            ns_per_series: 150,
            ns_per_call: 10_000,
            ns_per_install: 0,
            engine_reported_cpu_ns: None,
            cpu_accounting: CpuAccounting::CallingThread,
            faults: Vec::new(),
            installs: Cell::new(0),
            server: None,
        }
    }
}

impl FakeEngine {
    fn has(&self, fault: FakeFault) -> bool {
        self.faults.contains(&fault)
    }
}

/// The calibration target of the fake: ingesting `size` samples.
const INGEST_TARGET: &str = "fake/ingest";

impl EngineUnderTest for FakeEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "fake".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            version_dirty: false,
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .into(),
        }
    }

    fn cpu_accounting(&self) -> CpuAccounting {
        match &self.server {
            Some(server) => match &server.cgroup_dir {
                Some(dir) => CpuAccounting::Cgroup(CgroupSource::Local(dir.clone())),
                None => CpuAccounting::Processes(vec![server.pid]),
            },
            None => self.cpu_accounting.clone(),
        }
    }

    /// Reads each query's PromQL text, `sum_over_time(<metric>[...])` or
    /// `count_over_time(<metric>[...])`, over the query's `window_ms`.
    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        self.installs.set(self.installs.get() + 1);
        let plan = FakePlanFile::read(arm)?;
        match &self.server {
            Some(server) => server.charge(self.ns_per_install),
            None => self.clock.charge_thread(self.ns_per_install),
        }
        let mut queries = Vec::new();
        let mut scanned_metrics: Vec<String> = Vec::new();
        for query in &workload.queries {
            let text = query
                .texts
                .iter()
                .find(|text| text.language == QueryLanguage::PromQl)
                .ok_or_else(|| EngineError(format!("query {} has no PromQL text", query.id)))?;
            let (function, metric) = parse_over_time(&text.text)?;
            if !scanned_metrics.contains(&metric) {
                scanned_metrics.push(metric.clone());
            }
            queries.push(FakeQuery {
                id: query.id.clone(),
                function,
                metric,
                window_ms: query.window_ms as i64,
            });
        }
        Ok(Box::new(FakeArm {
            clock: self.clock.clone(),
            server: self.server.clone(),
            ns_per_sample: self.ns_per_sample,
            ns_per_series: self.ns_per_series,
            ns_per_call: self.ns_per_call,
            engine_reported_cpu_ns: self.engine_reported_cpu_ns,
            counts_window_start: self.has(FakeFault::CountsWindowStart),
            works_on_another_thread: self.has(FakeFault::WorksOnAnotherThread),
            answer_offset: if self.has(FakeFault::NondeterministicAnswers)
                && self.installs.get() >= 2
            {
                0.5
            } else {
                0.0
            },
            queries,
            scanned_metrics,
            plan,
            stored: BTreeMap::new(),
            node_cpu_ns_per_batch: BTreeMap::new(),
        }))
    }

    fn calibration(&mut self) -> Option<&mut dyn Calibratable> {
        Some(self)
    }
}

/// One calibration target, `fake/ingest`: `ns_per_sample` over `samples`.
impl Calibratable for FakeEngine {
    fn cost_targets(&self) -> Vec<CostTarget> {
        vec![CostTarget {
            key: serde_json::Value::String(INGEST_TARGET.into()),
            constant: "ns_per_sample".into(),
            unit: "samples".into(),
            sizes: vec![1_000, 10_000, 100_000],
        }]
    }

    /// The closure charges one ingest of `size` samples and returns `size`.
    fn prepare_cost_target(
        &mut self,
        target: &CostTarget,
        size: u64,
    ) -> Result<Box<dyn FnMut() -> Result<u64, EngineError>>, EngineError> {
        if target.key.as_str() != Some(INGEST_TARGET) {
            return Err(EngineError(format!("unknown cost target {}", target.key)));
        }
        let clock = self.clock.clone();
        let ns = self.ns_per_call + self.ns_per_sample * size;
        Ok(Box::new(move || {
            clock.charge_thread(ns);
            Ok(size)
        }))
    }

    fn charged_units(&self, target: &CostTarget, size: u64) -> f64 {
        let _ = target;
        if self.has(FakeFault::CalibrationUnitsDouble) {
            2.0 * size as f64
        } else {
            size as f64
        }
    }
}

/// The two functions the fake answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverTime {
    Sum,
    Count,
}

/// `sum_over_time(m[10s])` is `(Sum, "m")`.
fn parse_over_time(text: &str) -> Result<(OverTime, String), EngineError> {
    let unsupported = || {
        EngineError(format!(
            "the fake engine reads sum_over_time(<metric>[<w>]) or count_over_time(<metric>[<w>]), not {text}"
        ))
    };
    let (function, rest) = text.trim().split_once('(').ok_or_else(unsupported)?;
    let function = match function.trim() {
        "sum_over_time" => OverTime::Sum,
        "count_over_time" => OverTime::Count,
        _ => return Err(unsupported()),
    };
    let (metric, _) = rest.split_once('[').ok_or_else(unsupported)?;
    let metric = metric.trim();
    if metric.is_empty() {
        return Err(unsupported());
    }
    Ok((function, metric.to_string()))
}

/// The plan file an arm names in `engine_options.plan_file`, as the fake
/// reads it: `{}` installs normally, `{"refuse": "<message>"}` fails the
/// install with that message, and `{"substitutions": ["..."]}` reports
/// those substitutions, as a translating engine would.
#[derive(Debug, Clone, Default)]
struct FakePlanFile {
    path: Option<String>,
    substitutions: Vec<String>,
}

impl FakePlanFile {
    fn read(arm: &ArmSpec) -> Result<FakePlanFile, EngineError> {
        let Some(path) = arm.engine_options.get("plan_file").and_then(|p| p.as_str()) else {
            return Ok(FakePlanFile::default());
        };
        let text = std::fs::read_to_string(path)
            .map_err(|error| EngineError(format!("plan file {path}: {error}")))?;
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| EngineError(format!("plan file {path}: {error}")))?;
        if let Some(message) = json.get("refuse").and_then(|m| m.as_str()) {
            return Err(EngineError(message.to_string()));
        }
        let substitutions = json
            .get("substitutions")
            .and_then(|list| list.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(FakePlanFile {
            path: Some(path.to_string()),
            substitutions,
        })
    }
}

/// One installed query.
#[derive(Debug, Clone)]
struct FakeQuery {
    id: String,
    function: OverTime,
    metric: String,
    window_ms: i64,
}

/// Per series of one metric, its `(ts_ms, value)` samples in time order.
type StoredSeries = BTreeMap<Arc<SeriesLabels>, Vec<(i64, f64)>>;

/// An installed arm of the fake.
struct FakeArm {
    clock: FakeClock,
    server: Option<FakeServer>,
    ns_per_sample: u64,
    ns_per_series: u64,
    ns_per_call: u64,
    engine_reported_cpu_ns: Option<u64>,
    counts_window_start: bool,
    works_on_another_thread: bool,
    /// Added to every answer: 0.5 from the second install on under
    /// [`FakeFault::NondeterministicAnswers`].
    answer_offset: f64,
    queries: Vec<FakeQuery>,
    /// The metrics the shared `scan` node reads.
    scanned_metrics: Vec<String>,
    plan: FakePlanFile,
    /// Per metric, per series, its `(ts_ms, value)` samples in time order
    /// (chunks arrive in time order).
    stored: BTreeMap<String, StoredSeries>,
    /// Per node, its CPU in each evaluation since the last `node_profiles`.
    node_cpu_ns_per_batch: BTreeMap<String, Vec<u64>>,
}

impl FakeArm {
    /// Work on the calling thread, or on the server when there is one.
    fn charge_thread(&self, ns: u64) {
        match &self.server {
            Some(server) => server.charge(ns),
            None => self.clock.charge_thread(ns),
        }
    }

    /// Work on another thread, or on the server when there is one.
    fn charge_other_thread(&self, ns: u64) {
        match &self.server {
            Some(server) => server.charge(ns),
            None => self.clock.charge_other_thread(ns),
        }
    }

    /// Charges `ns` on the calling thread to `node` for this batch.
    fn charge_node(&mut self, node: &str, ns: u64) {
        self.charge_thread(ns);
        self.node_cpu_ns_per_batch
            .entry(node.to_string())
            .or_default()
            .push(ns);
    }

    fn report(&self) -> StepReport {
        StepReport {
            scanned_bytes: None,
            engine_reported_cpu_ns: self.engine_reported_cpu_ns,
        }
    }
}

impl InstalledArm for FakeArm {
    fn plan_summary(&self) -> PlanSummary {
        let mut lines = vec![format!("scan = scan({})", self.scanned_metrics.join(", "))];
        let mut node_kinds = vec!["scan".to_string()];
        for query in &self.queries {
            let kind = match query.function {
                OverTime::Sum => "sum_over_time",
                OverTime::Count => "count_over_time",
            };
            lines.push(format!(
                "{} = {kind}(scan.{}, {} ms)",
                query.id, query.metric, query.window_ms
            ));
            node_kinds.push(kind.to_string());
        }
        if let Some(path) = &self.plan.path {
            lines.push(format!("plan_file = {path}"));
        }
        PlanSummary {
            text: lines.join("\n"),
            node_kinds,
            stored_outputs: Vec::new(),
            substitutions: self.plan.substitutions.clone(),
            sharing_lost: Vec::new(),
        }
    }

    fn ingest(&mut self, chunk: &SampleChunk) -> Result<StepReport, EngineError> {
        if self.works_on_another_thread {
            self.charge_thread(1_000_000);
            self.charge_other_thread(8_000_000);
        } else {
            self.charge_thread(self.ns_per_call + self.ns_per_sample * chunk.samples.len() as u64);
        }
        for sample in &chunk.samples {
            let metric = sample.labels.get("__name__").cloned().unwrap_or_default();
            self.stored
                .entry(metric)
                .or_default()
                .entry(Arc::clone(&sample.labels))
                .or_default()
                .push((sample.ts_ms, sample.value));
        }
        Ok(self.report())
    }

    fn evaluate(&mut self, at_ms: i64) -> Result<(Answers, StepReport), EngineError> {
        self.charge_node("scan", self.ns_per_call);
        let mut answers: BTreeMap<String, BTreeMap<SeriesLabels, f64>> = BTreeMap::new();
        for query in self.queries.clone() {
            let start_ms = at_ms - query.window_ms;
            // The first sample in the window: after its start, or on it
            // under `CountsWindowStart`.
            let in_window_from =
                |ts_ms: i64| ts_ms > start_ms || (self.counts_window_start && ts_ms == start_ms);
            let mut answered_series = 0;
            for (labels, samples) in self.stored.get(&query.metric).into_iter().flatten() {
                // Samples are stored in time order, so the window is a slice.
                let first = samples.partition_point(|&(ts_ms, _)| !in_window_from(ts_ms));
                let end = samples.partition_point(|&(ts_ms, _)| ts_ms <= at_ms);
                let values: Vec<f64> = samples[first..end.max(first)]
                    .iter()
                    .map(|(_, value)| *value)
                    .collect();
                if values.is_empty() {
                    continue;
                }
                answered_series += 1;
                let value = match query.function {
                    OverTime::Sum => values.iter().sum(),
                    OverTime::Count => values.len() as f64,
                };
                // A range function drops the metric name (PromQL's output labels).
                let mut output_labels = SeriesLabels::clone(labels);
                output_labels.remove("__name__");
                answers
                    .entry(query.id.clone())
                    .or_default()
                    .insert(output_labels, value + self.answer_offset);
            }
            self.charge_node(&query.id, self.ns_per_series * answered_series);
        }
        Ok((Answers(answers), self.report()))
    }

    /// Each stored sample as 16 bytes (timestamp and value).
    fn footprint(&self) -> Result<Footprint, EngineError> {
        let samples: usize = self
            .stored
            .values()
            .flat_map(|series| series.values())
            .map(Vec::len)
            .sum();
        Ok(Footprint {
            view_bytes: 0,
            raw_retained_bytes: 16 * samples as u64,
            peak_bytes: None,
        })
    }

    fn node_profiles(&mut self) -> Option<Vec<NodeProfile>> {
        Some(
            std::mem::take(&mut self.node_cpu_ns_per_batch)
                .into_iter()
                .map(|(node, cpu_ns_per_batch)| NodeProfile {
                    node,
                    cpu_ns_per_batch,
                })
                .collect(),
        )
    }
}

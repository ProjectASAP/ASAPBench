use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use aqpbm_datagen::GeneratedTimeSeries;
use serde::{Deserialize, Serialize};

use crate::harness::cgroup::CgroupMemory;
use crate::harness::data::{DataRecord, Sample, SeriesLabels};
use crate::harness::engine::{
    ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, SampleWriter,
    WorkloadSpec,
};
use crate::harness::protocol::{
    processes_memory, CpuClock, MeasurementClocks, OsCpuClock, ProcessesMemory, ProtocolError,
};

const BATCHES_AHEAD: usize = 8;
const LONGEST_SLEEP: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteLoad {
    pub writers: usize,
    pub samples_per_request: usize,
    pub target_samples_per_second: Option<f64>,
    pub max_event_time_lead_ms: u64,
    pub warmup_ms: u64,
    pub measure_ms: u64,
    pub stop_grace_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteThroughputResult {
    pub engine: EngineDescription,
    pub workload: String,
    pub arm: String,
    pub series: u64,
    pub load: WriteLoad,
    pub acknowledged_samples_per_second: f64,
    pub engine_cpu_cores: f64,
    pub harness_cpu_cores: f64,
    pub measured_ms: f64,
    pub cpu_source: String,
    pub server_memory_after: Option<CgroupMemory>,
    pub server_process_memory_after: Option<ProcessesMemory>,
    pub widest_event_time_lead_ms: i64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WriteThroughputError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("{0}")]
    Load(String),
}

struct Series {
    records: Vec<RecordSeries>,
    count: usize,
    start_ms: i64,
}

struct RecordSeries {
    metric: String,
    generated: GeneratedTimeSeries,
    labels: Vec<Arc<SeriesLabels>>,
    first_index: usize,
}

impl Series {
    fn generate(data: &[DataRecord]) -> Result<Series, WriteThroughputError> {
        let mut records = Vec::with_capacity(data.len());
        let mut count = 0;
        for record in data {
            let mut generated = record.description.generate().map_err(|error| {
                WriteThroughputError::Load(format!("metric {}: {error}", record.metric))
            })?;
            let labels: Vec<Arc<SeriesLabels>> = std::mem::take(&mut generated.labels)
                .into_iter()
                .map(|pairs| {
                    let mut labels: SeriesLabels = pairs.into_iter().collect();
                    labels.insert("__name__".into(), record.metric.clone());
                    Arc::new(labels)
                })
                .collect();
            let first_index = count;
            count += labels.len();
            records.push(RecordSeries {
                metric: record.metric.clone(),
                generated,
                labels,
                first_index,
            });
        }
        let start_ms = data
            .iter()
            .map(|record| record.description.start_ms)
            .min()
            .unwrap_or(0);
        Ok(Series {
            records,
            count,
            start_ms,
        })
    }
}

struct Shared {
    stop: AtomicBool,
    acknowledged_samples: AtomicU64,
    event_time_ms: Vec<AtomicI64>,
    data_ran_out: AtomicBool,
    widest_lead_ms: AtomicI64,
    event_time_changed: Condvar,
    event_time_lock: Mutex<()>,
    error: Mutex<Option<WriteThroughputError>>,
}

impl Shared {
    fn slowest_event_time_ms(&self) -> i64 {
        self.event_time_ms
            .iter()
            .map(|time| time.load(Ordering::Relaxed))
            .min()
            .unwrap_or(i64::MAX)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn fail(&self, error: WriteThroughputError) {
        self.error.lock().unwrap().get_or_insert(error);
        self.stop.store(true, Ordering::Relaxed);
        self.event_time_changed.notify_all();
    }

    fn set_event_time(&self, writer: usize, ms: i64) {
        let _guard = self.event_time_lock.lock().unwrap();
        let slowest_before = self.slowest_event_time_ms();
        self.event_time_ms[writer].store(ms, Ordering::Relaxed);
        if self.slowest_event_time_ms() != slowest_before {
            self.event_time_changed.notify_all();
        }
    }

    fn wait_within_lead(&self, first_ms: i64, max_lead_ms: i64) {
        let mut guard = self.event_time_lock.lock().unwrap();
        while !self.stopped() && first_ms > self.slowest_event_time_ms().saturating_add(max_lead_ms)
        {
            guard = self
                .event_time_changed
                .wait_timeout(guard, LONGEST_SLEEP)
                .unwrap()
                .0;
        }
    }

    fn run_out(&self) {
        if !self.stopped() {
            self.data_ran_out.store(true, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            self.event_time_changed.notify_all();
        }
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_string())
}

fn spawn_watched(
    name: String,
    shared: Arc<Shared>,
    done: std::sync::mpsc::Sender<()>,
    body: impl FnOnce(&Shared) + Send + 'static,
) {
    std::thread::spawn(move || {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&shared)))
        {
            shared.fail(WriteThroughputError::Load(format!(
                "{name} panicked: {}",
                panic_text(payload.as_ref())
            )));
        }
        let _ = done.send(());
    });
}

fn generate_batches(
    series: &Series,
    slice: Range<usize>,
    samples_per_request: usize,
    batches: SyncSender<Vec<Sample>>,
    shared: &Shared,
) {
    let mut streams = Vec::new();
    for record in &series.records {
        let start = slice.start.max(record.first_index);
        let end = slice.end.min(record.first_index + record.labels.len());
        if start >= end {
            continue;
        }
        match record
            .generated
            .samples_of_series(start - record.first_index..end - record.first_index)
        {
            Ok(samples) => streams.push((record, samples.peekable())),
            Err(error) => {
                shared.fail(WriteThroughputError::Load(format!(
                    "metric {}: {error}",
                    record.metric
                )));
                return;
            }
        }
    }
    let capacity = samples_per_request.min(slice.len());
    let mut batch = Vec::with_capacity(capacity);
    loop {
        if streams
            .iter_mut()
            .any(|(_, samples)| samples.peek().is_none())
        {
            shared.run_out();
            return;
        }
        let next = streams
            .iter_mut()
            .enumerate()
            .filter_map(|(stream, (_, samples))| {
                samples.peek().map(|sample| match sample {
                    Ok((_, ts_ms, _)) => (*ts_ms, stream),
                    Err(_) => (i64::MIN, stream),
                })
            })
            .min();
        let Some((_, stream)) = next else {
            break;
        };
        let (record, samples) = &mut streams[stream];
        let (index, ts_ms, value) = match samples.next().expect("peeked") {
            Ok(sample) => sample,
            Err(error) => {
                shared.fail(WriteThroughputError::Load(format!(
                    "metric {}: {error}",
                    record.metric
                )));
                return;
            }
        };
        if batch
            .last()
            .is_some_and(|last: &Sample| last.ts_ms != ts_ms)
        {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(capacity));
            if batches.send(full).is_err() {
                return;
            }
        }
        batch.push(Sample {
            labels: Arc::clone(&record.labels[index]),
            ts_ms,
            value,
        });
        if batch.len() == samples_per_request {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(capacity));
            if batches.send(full).is_err() {
                return;
            }
        }
    }
    if !batch.is_empty() {
        let _ = batches.send(batch);
    }
    shared.run_out();
}

fn sleep_until(due: Instant, shared: &Shared) {
    loop {
        let now = Instant::now();
        if now >= due || shared.stopped() {
            return;
        }
        std::thread::sleep((due - now).min(LONGEST_SLEEP));
    }
}

fn sleep_until_stopped(shared: &Shared) {
    while !shared.stopped() {
        std::thread::sleep(LONGEST_SLEEP);
    }
}

fn next_batch(batches: &Receiver<Vec<Sample>>, shared: &Shared) -> Option<Vec<Sample>> {
    loop {
        if shared.stopped() {
            return None;
        }
        match batches.recv_timeout(LONGEST_SLEEP) {
            Ok(batch) => return Some(batch),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                shared.run_out();
                return None;
            }
        }
    }
}

fn write_batches(
    mut writer: Box<dyn SampleWriter>,
    index: usize,
    samples_per_second: Option<f64>,
    max_lead_ms: i64,
    started: Instant,
    batches: Receiver<Vec<Sample>>,
    shared: &Shared,
) {
    let mut written: u64 = 0;
    while let Some(batch) = next_batch(&batches, shared) {
        let first_ms = batch.first().map_or(i64::MIN, |sample| sample.ts_ms);
        shared.set_event_time(index, first_ms);
        shared.wait_within_lead(first_ms, max_lead_ms);
        if shared.stopped() {
            break;
        }
        if let Some(rate) = samples_per_second {
            match Duration::try_from_secs_f64(written as f64 / rate) {
                Ok(offset) => sleep_until(started + offset, shared),
                Err(_) => sleep_until_stopped(shared),
            }
            if shared.stopped() {
                break;
            }
        }
        shared.widest_lead_ms.fetch_max(
            first_ms.saturating_sub(shared.slowest_event_time_ms()),
            Ordering::Relaxed,
        );
        if let Err(error) = writer.write(&batch) {
            shared.fail(error.into());
            break;
        }
        written += batch.len() as u64;
        shared
            .acknowledged_samples
            .fetch_add(batch.len() as u64, Ordering::Relaxed);
    }
    shared.set_event_time(index, i64::MAX);
}

fn check_load(load: &WriteLoad) -> Result<(), WriteThroughputError> {
    let problem = if load.writers == 0 {
        Some("writers must be at least 1".to_string())
    } else if load.samples_per_request == 0 {
        Some("samples_per_request must be at least 1".to_string())
    } else if load.measure_ms == 0 {
        Some("measure_ms must be above 0".to_string())
    } else if load.max_event_time_lead_ms > i64::MAX as u64 {
        Some("max_event_time_lead_ms is too large".to_string())
    } else {
        match load.target_samples_per_second {
            Some(rate) if !(rate.is_finite() && rate > 0.0) => Some(format!(
                "target_samples_per_second must be above 0, not {rate}"
            )),
            _ => None,
        }
    };
    match problem {
        Some(problem) => Err(WriteThroughputError::Load(problem)),
        None => Ok(()),
    }
}

pub fn run_write_throughput(
    engine: &dyn EngineUnderTest,
    workload: &WorkloadSpec,
    arm: &ArmSpec,
    load: &WriteLoad,
    clocks: &MeasurementClocks,
) -> Result<WriteThroughputResult, WriteThroughputError> {
    check_load(load)?;
    let description = engine.description();
    let accounting = engine.cpu_accounting();
    if accounting == CpuAccounting::CallingThread {
        return Err(WriteThroughputError::Load(format!(
            "engine {} runs in the harness process, so its CPU cannot be told apart from the writers'",
            description.id
        )));
    }
    if workload.data.is_empty() {
        return Err(WriteThroughputError::Load(format!(
            "workload {} has no data",
            workload.name
        )));
    }
    let first = &workload.data[0].description;
    if let Some(other) = workload.data.iter().find(|record| {
        record.description.start_ms != first.start_ms
            || record.description.sample_interval_ms != first.sample_interval_ms
    }) {
        return Err(WriteThroughputError::Load(format!(
            "metric {} starts at {} ms every {} ms, metric {} at {} ms every {} ms: every metric \
             of a write-throughput workload needs the same start and sample interval, so event \
             time advances alike in every writer",
            workload.data[0].metric,
            first.start_ms,
            first.sample_interval_ms,
            other.metric,
            other.description.start_ms,
            other.description.sample_interval_ms
        )));
    }
    let series = Arc::new(Series::generate(&workload.data)?);
    if series.count < load.writers {
        return Err(WriteThroughputError::Load(format!(
            "{} writers need at least as many series; workload {} has {}",
            load.writers, workload.name, series.count
        )));
    }
    let clock = clocks.clock_for(&accounting)?;
    let mut installed = engine.install(workload, arm)?;
    let writers = installed.sample_writers(load.writers).ok_or_else(|| {
        WriteThroughputError::Load(format!(
            "engine {} has no sample writers, so it cannot be loaded concurrently",
            description.id
        ))
    })?;
    if writers.len() != load.writers {
        return Err(WriteThroughputError::Load(format!(
            "engine {} gave {} sample writers for {} requested",
            description.id,
            writers.len(),
            load.writers
        )));
    }

    let start_ms = series.start_ms;
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        acknowledged_samples: AtomicU64::new(0),
        event_time_ms: (0..load.writers)
            .map(|_| AtomicI64::new(start_ms))
            .collect(),
        data_ran_out: AtomicBool::new(false),
        widest_lead_ms: AtomicI64::new(0),
        event_time_changed: Condvar::new(),
        event_time_lock: Mutex::new(()),
        error: Mutex::new(None),
    });
    let started = Instant::now();
    let (done, finished) = std::sync::mpsc::channel();
    for (index, writer) in writers.into_iter().enumerate() {
        let slice = index * series.count / load.writers..(index + 1) * series.count / load.writers;
        let samples_per_second = load
            .target_samples_per_second
            .map(|rate| rate * slice.len() as f64 / series.count as f64);
        let (sender, receiver) = sync_channel(BATCHES_AHEAD);
        let generator_series = Arc::clone(&series);
        let samples_per_request = load.samples_per_request;
        spawn_watched(
            format!("the generator of writer {index}"),
            Arc::clone(&shared),
            done.clone(),
            move |shared| {
                generate_batches(
                    &generator_series,
                    slice,
                    samples_per_request,
                    sender,
                    shared,
                )
            },
        );
        let max_lead_ms = load.max_event_time_lead_ms as i64;
        spawn_watched(
            format!("writer {index}"),
            Arc::clone(&shared),
            done.clone(),
            move |shared| {
                write_batches(
                    writer,
                    index,
                    samples_per_second,
                    max_lead_ms,
                    started,
                    receiver,
                    shared,
                )
            },
        );
    }
    drop(done);

    let engine_clock = clock.as_clock();
    let harness_clock = OsCpuClock;
    sleep_until(started + Duration::from_millis(load.warmup_ms), &shared);
    let acknowledged_before = shared.acknowledged_samples.load(Ordering::Relaxed);
    let acknowledged_started = Instant::now();
    let engine_cpu_before = engine_clock.process_cpu_ns();
    let engine_started = Instant::now();
    let harness_cpu_before = harness_clock.process_cpu_ns();
    let harness_started = Instant::now();
    sleep_until(
        acknowledged_started + Duration::from_millis(load.measure_ms),
        &shared,
    );
    let acknowledged_after = shared.acknowledged_samples.load(Ordering::Relaxed);
    let acknowledged_window = acknowledged_started.elapsed();
    let engine_cpu_after = engine_clock.process_cpu_ns();
    let engine_window = engine_started.elapsed();
    let harness_cpu_after = harness_clock.process_cpu_ns();
    let harness_window = harness_started.elapsed();
    let stopped_early = shared.stopped();
    shared.stop.store(true, Ordering::Relaxed);
    shared.event_time_changed.notify_all();
    let grace_ends = Instant::now() + Duration::from_millis(load.stop_grace_ms);
    let mut running = 2 * load.writers;
    while running > 0 {
        let left = grace_ends.saturating_duration_since(Instant::now());
        match finished.recv_timeout(left) {
            Ok(()) => running -= 1,
            Err(_) => break,
        }
    }

    if let Some(error) = shared.error.lock().unwrap().take() {
        return Err(error);
    }
    if running > 0 {
        return Err(WriteThroughputError::Load(format!(
            "{running} writer or generator threads were still running {} ms after the stop: a \
             write did not return",
            load.stop_grace_ms
        )));
    }
    if let Some(error) = engine_clock.take_read_error() {
        return Err(error.into());
    }
    if shared.data_ran_out.load(Ordering::Relaxed) {
        return Err(WriteThroughputError::Load(format!(
            "workload {} ran out of samples before the measurement ended; lengthen its duration_ms",
            workload.name
        )));
    }
    if stopped_early {
        return Err(WriteThroughputError::Load(
            "the load stopped before the measurement ended".into(),
        ));
    }
    let (server_memory_after, server_process_memory_after) = match &accounting {
        CpuAccounting::Processes(pids) => (
            None,
            Some(processes_memory(&clocks.process_cpu_files.proc_root, pids)?),
        ),
        CpuAccounting::Cgroup(source) => {
            (Some(source.memory().map_err(ProtocolError::from)?), None)
        }
        CpuAccounting::CallingThread => (None, None),
    };
    drop(installed);

    let seconds = acknowledged_window.as_secs_f64();
    let cores = |before: u64, after: u64, window: Duration| {
        after.saturating_sub(before) as f64 / 1e9 / window.as_secs_f64()
    };
    Ok(WriteThroughputResult {
        engine: description,
        workload: workload.name.clone(),
        arm: arm.name.clone(),
        series: series.count as u64,
        load: load.clone(),
        acknowledged_samples_per_second: (acknowledged_after - acknowledged_before) as f64
            / seconds,
        engine_cpu_cores: cores(engine_cpu_before, engine_cpu_after, engine_window),
        harness_cpu_cores: cores(harness_cpu_before, harness_cpu_after, harness_window),
        measured_ms: seconds * 1e3,
        cpu_source: clock.cpu_source(),
        server_memory_after,
        server_process_memory_after,
        widest_event_time_lead_ms: shared.widest_lead_ms.load(Ordering::Relaxed),
    })
}

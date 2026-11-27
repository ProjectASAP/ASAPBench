use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aqpbm_core::harness::data::Sample;
use aqpbm_core::harness::engine::{
    ArmSpec, EngineUnderTest, QueryLanguage, QueryText, SampleChunk, SeriesLabels, WorkloadQuery,
    WorkloadSpec,
};
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use prost::Message;
use serde_json::json;
use system_bench::engines_external::asapquery_backend_server::{
    AsapqueryBackendEngine, AsapqueryBackendServer,
};
use system_bench::engines_external::asapquery_precompute_engine_server::{
    AsapqueryPrecomputeEngine, AsapqueryPrecomputeEngineServer,
};
use system_bench::engines_external::victoriametrics_server::{
    VictoriaMetricsEngine, VictoriaMetricsServer,
};
use system_bench::prometheus_api::remote_write::WriteRequest;

struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

fn recording_server() -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let busy_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut length = 0;
            let mut header = String::new();
            while reader.read_line(&mut header).unwrap() > 0 && header.trim() != "" {
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                header.clear();
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut words = line.split_whitespace();
            let method = words.next().unwrap_or("").to_string();
            let target = words.next().unwrap_or("").to_string();
            let query = target
                .split_once("query=")
                .map(|(_, rest)| rest.split('&').next().unwrap_or("").to_string())
                .unwrap_or_default();
            let (status, answer) = if target == "/api/v1/write"
                && busy_once.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                (503, String::new())
            } else if target.starts_with("/api/v1/query") && query.contains("duplicate") {
                (
                    200,
                    "{\"status\":\"success\",\"data\":{\"resultType\":\"vector\",\"result\":[\
                     {\"metric\":{\"__name__\":\"m\",\"job\":\"a\"},\"value\":[60,\"1\"]},\
                     {\"metric\":{\"__name__\":\"n\",\"job\":\"a\"},\"value\":[60,\"2\"]}]}}"
                        .to_string(),
                )
            } else if target.starts_with("/api/v1/query") {
                (
                    200,
                    "{\"status\":\"success\",\"data\":{\"resultType\":\"vector\",\"result\":[\
                     {\"metric\":{\"__name__\":\"m\",\"job\":\"a\"},\"value\":[60,\"42\"]},\
                     {\"metric\":{\"job\":\"b\"},\"value\":[60,\"NaN\"]}]}}"
                        .to_string(),
                )
            } else if target.starts_with("/api/v1/status/asap") {
                (200, "{\"engine_running\":true}".to_string())
            } else if target.starts_with("/metrics") {
                (
                    200,
                    "vm_app_version{version=\"victoria-metrics-fake-v1.154.1\"} 1\n\
                     vm_data_size_bytes{type=\"storage/small\"} 0\n\
                     asap_raw_data_bytes 100\n\
                     asap_view_bytes 40\n"
                        .to_string(),
                )
            } else {
                (200, String::new())
            };
            log.lock().unwrap().push(Request {
                method,
                target,
                body,
            });
            let _ = write!(
                stream,
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (url, seen)
}

fn workload() -> WorkloadSpec {
    WorkloadSpec {
        name: "w".into(),
        queries: [("q", "max_over_time(m[1m])"), ("selector", "m{job=\"a\"}")]
            .into_iter()
            .map(|(id, text)| WorkloadQuery {
                id: id.into(),
                texts: vec![QueryText {
                    language: QueryLanguage::PromQl,
                    text: text.into(),
                }],
                window_ms: 60_000,
                evaluation_interval_ms: 60_000,
                truth: None,
            })
            .collect(),
        data: vec![aqpbm_core::harness::data::DataRecord {
            metric: "m".into(),
            description: TimeSeriesDescription {
                labels: vec![SeriesLabelSpec {
                    name: "job".into(),
                    cardinality: 1,
                    skewness: 0.0,
                }],
                series_num: 1,
                start_ms: 0,
                sample_interval_ms: 20_000,
                duration_ms: 60_000,
                value_distribution: DataDistribution::Uniform(UniformParameter {
                    lower_bound: 0.0,
                    upper_bound: 1.0,
                    seed: 0,
                }),
                counter: false,
                seed: 1,
            },
        }],
    }
}

fn query_text_arm() -> ArmSpec {
    ArmSpec {
        name: "normal".into(),
        raw_data_retained: true,
        engine_options: json!({}),
    }
}

fn labels(metric: &str) -> Arc<SeriesLabels> {
    Arc::new(
        [("__name__", metric), ("job", "a")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

fn chunk() -> SampleChunk {
    let late = labels("m");
    let early = labels("n");
    let mut samples: Vec<Sample> = (0..10_001)
        .map(|i| Sample {
            labels: Arc::clone(&late),
            ts_ms: 30_000 + i,
            value: 1.0,
        })
        .collect();
    samples.extend((0..10_000).map(|i| Sample {
        labels: Arc::clone(&early),
        ts_ms: 1 + i,
        value: 1.0,
    }));
    SampleChunk {
        end_ms: 60_000,
        samples,
    }
}

fn written_time_ranges(requests: &[Request]) -> Vec<(i64, i64)> {
    requests
        .iter()
        .filter(|r| r.method == "POST" && r.target == "/api/v1/write")
        .map(|r| {
            let raw = snap::raw::Decoder::new().decompress_vec(&r.body).unwrap();
            let request = WriteRequest::decode(raw.as_slice()).unwrap();
            let times: Vec<i64> = request
                .timeseries
                .iter()
                .flat_map(|series| series.samples.iter().map(|s| s.timestamp))
                .collect();
            (*times.iter().min().unwrap(), *times.iter().max().unwrap())
        })
        .collect()
}

fn check_writes(requests: &[Request]) {
    let ranges = written_time_ranges(requests);
    assert_eq!(ranges.len(), 4, "a busy answer, then three batches");
    let accepted = &ranges[1..];
    assert!(
        accepted.windows(2).all(|pair| pair[0].1 <= pair[1].0),
        "{ranges:?}"
    );
    assert_eq!(written_samples(requests) - 10_000, 20_001);
}

fn written_samples(requests: &[Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.method == "POST" && r.target == "/api/v1/write")
        .map(|r| {
            let raw = snap::raw::Decoder::new().decompress_vec(&r.body).unwrap();
            WriteRequest::decode(raw.as_slice())
                .unwrap()
                .timeseries
                .iter()
                .map(|series| series.samples.len())
                .sum::<usize>()
        })
        .sum()
}

fn check_answers(answers: &aqpbm_core::harness::engine::Answers) {
    let job = |job: &str| -> SeriesLabels {
        [("job".to_string(), job.to_string())].into_iter().collect()
    };
    let function = &answers.0["q"];
    assert_eq!(function.get(&job("a")), Some(&42.0));
    assert!(function.get(&job("b")).unwrap().is_nan());
    let mut named = job("a");
    named.insert("__name__".into(), "m".into());
    assert_eq!(answers.0["selector"].get(&named), Some(&42.0));
}

fn duplicate_workload() -> WorkloadSpec {
    let mut workload = workload();
    workload.queries.truncate(1);
    workload.queries[0].texts[0].text = "duplicate_over_time(m[1m])".into();
    workload
}

fn cgroup() -> aqpbm_core::harness::cgroup::CgroupSource {
    aqpbm_core::harness::cgroup::CgroupSource::Local(std::env::temp_dir())
}

#[test]
fn asapquery_backend_writes_closes_panes_and_asks_the_promql() {
    let (url, seen) = recording_server();
    let engine = AsapqueryBackendEngine::new(AsapqueryBackendServer {
        url,
        cgroup: cgroup(),
        request_timeout: Duration::from_secs(5),
    })
    .unwrap();
    let with_plan = ArmSpec {
        engine_options: json!({ "plan_file": "p.plan.json" }),
        ..query_text_arm()
    };
    assert!(engine.install(&workload(), &with_plan).is_err());
    let mut arm = engine.install(&workload(), &query_text_arm()).unwrap();
    arm.ingest(&chunk()).unwrap();
    let (answers, _) = arm.evaluate(60_000).unwrap();
    check_answers(&answers);
    let footprint = arm.footprint().unwrap();
    assert_eq!(
        (footprint.view_bytes, footprint.raw_retained_bytes),
        (40, 100)
    );
    let requests = seen.lock().unwrap();
    check_writes(&requests);
    let last_write = requests
        .iter()
        .rposition(|r| r.target == "/api/v1/write")
        .unwrap();
    let watermark = requests
        .iter()
        .position(|r| r.target == "/api/v1/precompute/watermark?time_ms=60000")
        .unwrap();
    assert!(last_write < watermark);
    assert!(requests
        .iter()
        .any(|r| r.target.starts_with("/api/v1/query?") && r.target.contains("time=60.000")));
    drop(requests);
    drop(arm);
    assert!(engine.install(&workload(), &query_text_arm()).is_err());
}

#[test]
fn two_answers_for_one_series_are_refused() {
    let (url, _) = recording_server();
    let engine = VictoriaMetricsEngine::new(VictoriaMetricsServer {
        url,
        cgroup: cgroup(),
        stream_aggregation_config: None,
        request_timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut arm = engine
        .install(&duplicate_workload(), &query_text_arm())
        .unwrap();
    let error = arm.evaluate(60_000).err().unwrap();
    assert!(error.0.contains("two answers"), "{}", error.0);
}

#[test]
fn victoriametrics_without_a_plan_writes_the_samples_and_asks_the_query_text() {
    let (url, seen) = recording_server();
    let engine = VictoriaMetricsEngine::new(VictoriaMetricsServer {
        url,
        cgroup: cgroup(),
        stream_aggregation_config: None,
        request_timeout: Duration::from_secs(5),
    })
    .unwrap();
    let with_plan = ArmSpec {
        engine_options: json!({ "plan_file": "p.plan.json" }),
        ..query_text_arm()
    };
    let refused = engine.install(&workload(), &with_plan).err().unwrap();
    assert!(refused.0.contains("config"), "{}", refused.0);
    let mut arm = engine.install(&workload(), &query_text_arm()).unwrap();
    assert!(engine.install(&workload(), &query_text_arm()).is_err());
    arm.ingest(&chunk()).unwrap();
    let (answers, _) = arm.evaluate(60_000).unwrap();
    check_answers(&answers);
    drop(arm);
    drop(engine.install(&workload(), &query_text_arm()).unwrap());
    let requests = seen.lock().unwrap();
    check_writes(&requests);
    assert!(requests.iter().any(|r| r.method == "POST"
        && r.target.starts_with("/api/v1/admin/tsdb/delete_series?")
        && r.target.contains("__name__")));
    let deletes = requests
        .iter()
        .filter(|r| r.target.starts_with("/api/v1/admin/tsdb/delete_series?"))
        .count();
    assert_eq!(deletes, 4);
    assert!(requests
        .iter()
        .any(|r| r.target.starts_with("/api/v1/query?") && r.target.contains("nocache=1")));
    assert!(requests.iter().any(|r| r.target == "/internal/force_flush"));
}

fn streaming_config(test: &str, metric: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "system-bench-precompute-engine-{}-{test}.yaml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        format!(
            "aggregations:\n- aggregationId: 1\n  aggregationType: DatasketchesKLL\n  \
             metric: {metric}\n  windowType: tumbling\nmetrics:\n  {metric}: [job]\n"
        ),
    )
    .unwrap();
    path
}

fn precompute_engine_server(
    url: &str,
    streaming_config: std::path::PathBuf,
) -> AsapqueryPrecomputeEngineServer {
    AsapqueryPrecomputeEngineServer {
        url: url.into(),
        pids: vec![std::process::id()],
        streaming_config,
        version: "test".into(),
        request_timeout: Duration::from_secs(10),
    }
}

fn precompute_engine(url: &str, test: &str) -> AsapqueryPrecomputeEngine {
    let config = streaming_config(test, "m");
    let engine = AsapqueryPrecomputeEngine::new(precompute_engine_server(url, config.clone()));
    let _ = std::fs::remove_file(config);
    engine.unwrap()
}

fn workload_without_queries() -> WorkloadSpec {
    WorkloadSpec {
        queries: Vec::new(),
        ..workload()
    }
}

#[test]
fn asapquery_precompute_engine_writes_through_each_sample_writer() {
    let (url, seen) = recording_server();
    let engine = precompute_engine(&url, "writes");
    assert_eq!(
        engine.cpu_accounting(),
        aqpbm_core::harness::engine::CpuAccounting::Processes(vec![std::process::id()])
    );
    let mut arm = engine
        .install(&workload_without_queries(), &query_text_arm())
        .unwrap();
    let mut writers = arm.sample_writers(2).unwrap();
    assert_eq!(writers.len(), 2);
    let samples = chunk().samples;
    let busy = writers[0].write(&samples[..1]).unwrap_err();
    assert!(busy.0.contains("busy"), "{busy}");
    seen.lock().unwrap().clear();
    writers[0].write(&samples[..100]).unwrap();
    writers[1].write(&samples[100..300]).unwrap();
    let requests = seen.lock().unwrap();
    let writes: Vec<&Request> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.target == "/api/v1/write")
        .collect();
    assert_eq!(writes.len(), 2);
    assert_eq!(written_samples(&requests), 300);
    assert_eq!(engine.description().version, "test");
}

#[test]
fn asapquery_precompute_engine_answers_no_queries() {
    let (url, _) = recording_server();
    let engine = precompute_engine(&url, "no-queries");
    let error = engine
        .install(&workload(), &query_text_arm())
        .err()
        .unwrap();
    assert!(error.0.contains("answers no queries"), "{error}");
    let mut arm = engine
        .install(&workload_without_queries(), &query_text_arm())
        .unwrap();
    assert!(arm.evaluate(60_000).is_err());
    let plan_arm = ArmSpec {
        engine_options: json!({"plan_file": "plan.json"}),
        ..query_text_arm()
    };
    assert!(engine
        .install(&workload_without_queries(), &plan_arm)
        .is_err());
}

#[test]
fn asapquery_precompute_engine_needs_a_pid_and_a_listening_server() {
    let (url, _) = recording_server();
    let config = streaming_config("checks", "m");
    let no_pid = AsapqueryPrecomputeEngine::new(AsapqueryPrecomputeEngineServer {
        pids: Vec::new(),
        ..precompute_engine_server(&url, config.clone())
    });
    assert!(no_pid.is_err());
    let unused = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    let refused = AsapqueryPrecomputeEngine::new(precompute_engine_server(&closed, config.clone()));
    assert!(refused.is_err());
    let _ = std::fs::remove_file(config);
    let missing = AsapqueryPrecomputeEngine::new(precompute_engine_server(
        &url,
        std::env::temp_dir().join("system-bench-no-such-streaming-config.yaml"),
    ));
    assert!(missing.is_err());
}

#[test]
fn asapquery_precompute_engine_refuses_a_metric_its_streaming_config_does_not_aggregate() {
    let (url, _) = recording_server();
    let config = streaming_config("other-metric", "other_metric");
    let engine =
        AsapqueryPrecomputeEngine::new(precompute_engine_server(&url, config.clone())).unwrap();
    let _ = std::fs::remove_file(config);
    let error = engine
        .install(&workload_without_queries(), &query_text_arm())
        .err()
        .unwrap();
    assert!(error.0.contains("no aggregation"), "{error}");
}

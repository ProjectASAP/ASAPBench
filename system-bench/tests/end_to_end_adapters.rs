use std::collections::BTreeMap;
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
use system_bench::engines_external::victoriametrics_server::{
    VictoriaMetricsEngine, VictoriaMetricsServer,
};
use system_bench::prometheus_api::remote_write::WriteRequest;
use system_bench::systemeval::asapquery_backend_server::{
    AsapqueryBackendEngine, AsapqueryBackendServer,
};

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
            let answer = if target.starts_with("/api/v1/query") {
                "{\"status\":\"success\",\"data\":{\"resultType\":\"vector\",\"result\":[\
                 {\"metric\":{\"job\":\"a\"},\"value\":[60,\"42\"]},\
                 {\"metric\":{\"job\":\"b\"},\"value\":[60,\"NaN\"]}]}}"
                    .to_string()
            } else if target.starts_with("/api/v1/status/asap") {
                "{\"engine_running\":true}".to_string()
            } else if target.starts_with("/metrics") {
                "vm_app_version{version=\"victoria-metrics-fake-v1.154.1\"} 1\n\
                 vm_data_size_bytes{type=\"storage/small\"} 0\n"
                    .to_string()
            } else {
                String::new()
            };
            log.lock().unwrap().push(Request {
                method,
                target,
                body,
            });
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (url, seen)
}

fn workload() -> WorkloadSpec {
    WorkloadSpec {
        name: "w".into(),
        queries: vec![WorkloadQuery {
            id: "q".into(),
            texts: vec![QueryText {
                language: QueryLanguage::PromQl,
                text: "sum by (job) (sum_over_time(m[1m]))".into(),
            }],
            window_ms: 60_000,
            evaluation_interval_ms: 60_000,
            truth: None,
        }],
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

fn chunk() -> SampleChunk {
    let labels: Arc<SeriesLabels> = Arc::new(
        [("__name__", "m"), ("job", "a")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    SampleChunk {
        end_ms: 60_000,
        samples: [20_000, 40_000, 60_000]
            .into_iter()
            .map(|ts_ms| Sample {
                labels: Arc::clone(&labels),
                ts_ms,
                value: 1.0,
            })
            .collect(),
    }
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

fn expected_answers() -> BTreeMap<SeriesLabels, f64> {
    [(
        [("job".to_string(), "a".to_string())].into_iter().collect(),
        42.0,
    )]
    .into_iter()
    .collect()
}

#[test]
fn asapquery_backend_writes_closes_panes_and_asks_the_promql() {
    let (url, seen) = recording_server();
    let engine = AsapqueryBackendEngine::new(AsapqueryBackendServer {
        url,
        cgroup: None,
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
    assert_eq!(answers.0["q"], expected_answers());
    let requests = seen.lock().unwrap();
    assert_eq!(written_samples(&requests), 3);
    assert!(requests
        .iter()
        .any(|r| r.target == "/api/v1/precompute/watermark?time_ms=60000"));
    assert!(requests
        .iter()
        .any(|r| r.target.starts_with("/api/v1/query?") && r.target.contains("time=60.000")));
    drop(requests);
    drop(arm);
    assert!(engine.install(&workload(), &query_text_arm()).is_err());
}

#[test]
fn victoriametrics_without_a_plan_writes_the_samples_and_asks_the_query_text() {
    let (url, seen) = recording_server();
    let engine = VictoriaMetricsEngine::new(VictoriaMetricsServer {
        url,
        cgroup: None,
        stream_aggregation_config: None,
        request_timeout: Duration::from_secs(5),
    })
    .unwrap();
    let with_plan = ArmSpec {
        engine_options: json!({ "plan_file": "p.plan.json" }),
        ..query_text_arm()
    };
    let refused = engine.install(&workload(), &with_plan).err().unwrap();
    assert!(refused.0.contains("cgroup"), "{}", refused.0);
    let mut arm = engine.install(&workload(), &query_text_arm()).unwrap();
    arm.ingest(&chunk()).unwrap();
    let (answers, _) = arm.evaluate(60_000).unwrap();
    assert_eq!(answers.0["q"], expected_answers());
    drop(arm);
    let requests = seen.lock().unwrap();
    assert_eq!(written_samples(&requests), 3);
    let deletes = requests
        .iter()
        .filter(|r| r.target.starts_with("/api/v1/admin/tsdb/delete_series?"))
        .count();
    assert_eq!(deletes, 2);
    assert!(requests
        .iter()
        .any(|r| r.target.starts_with("/api/v1/query?") && r.target.contains("nocache=1")));
    assert!(requests.iter().any(|r| r.target == "/internal/force_flush"));
}

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use aqpbm_core::harness::data::{Sample as HarnessSample, SeriesLabels};
use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct WriteRequest {
    #[prost(message, repeated, tag = "1")]
    pub timeseries: Vec<TimeSeries>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TimeSeries {
    #[prost(message, repeated, tag = "1")]
    pub labels: Vec<Label>,
    #[prost(message, repeated, tag = "2")]
    pub samples: Vec<Sample>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Label {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Sample {
    #[prost(double, tag = "1")]
    pub value: f64,
    #[prost(int64, tag = "2")]
    pub timestamp: i64,
}

pub fn encode(request: &WriteRequest) -> Vec<u8> {
    snap::raw::Encoder::new()
        .compress_vec(&request.encode_to_vec())
        .expect("snappy compresses any buffer")
}

pub enum Sent {
    Accepted,
    Busy,
}

pub fn write_url(base: &str) -> String {
    format!("{base}/api/v1/write")
}

pub fn send(agent: &ureq::Agent, base: &str, body: &[u8]) -> Result<Sent> {
    send_to(agent, &write_url(base), body)
}

pub fn send_to(agent: &ureq::Agent, url: &str, body: &[u8]) -> Result<Sent> {
    let response = agent
        .post(url)
        .set("Content-Encoding", "snappy")
        .set("Content-Type", "application/x-protobuf")
        .set("X-Prometheus-Remote-Write-Version", "0.1.0")
        .send_bytes(body);
    match response {
        Ok(_) => Ok(Sent::Accepted),
        Err(ureq::Error::Status(429 | 503, _)) => Ok(Sent::Busy),
        Err(ureq::Error::Status(code, response)) => {
            let text = response.into_string().unwrap_or_default();
            bail!("{url} answered {code}: {text}");
        }
        Err(error) => Err(error).with_context(|| format!("writing to {url}")),
    }
}

pub fn send_until_accepted(agent: &ureq::Agent, base: &str, body: &[u8]) -> Result<u64> {
    let mut retries = 0;
    while let Sent::Busy = send(agent, base, body)? {
        retries += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(retries)
}

pub fn request_of(samples: &[HarnessSample]) -> WriteRequest {
    let mut per_series: BTreeMap<&SeriesLabels, Vec<Sample>> = BTreeMap::new();
    for sample in samples {
        per_series.entry(&sample.labels).or_default().push(Sample {
            value: sample.value,
            timestamp: sample.ts_ms,
        });
    }
    WriteRequest {
        timeseries: per_series
            .into_iter()
            .map(|(labels, samples)| TimeSeries {
                labels: labels
                    .iter()
                    .map(|(name, value)| Label {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
                samples,
            })
            .collect(),
    }
}

pub struct RemoteWriteEncoder {
    labels_of_series: HashMap<usize, (Arc<SeriesLabels>, Vec<Label>)>,
    snappy: snap::raw::Encoder,
    buffer: Vec<u8>,
}

impl Default for RemoteWriteEncoder {
    fn default() -> Self {
        RemoteWriteEncoder {
            labels_of_series: HashMap::new(),
            snappy: snap::raw::Encoder::new(),
            buffer: Vec::new(),
        }
    }
}

impl RemoteWriteEncoder {
    pub fn encode(&mut self, samples: &[HarnessSample]) -> Vec<u8> {
        let mut request = WriteRequest::default();
        let mut keys: Vec<usize> = Vec::new();
        let mut position_of_series: HashMap<usize, usize> = HashMap::new();
        for sample in samples {
            let key = Arc::as_ptr(&sample.labels) as usize;
            let position = *position_of_series.entry(key).or_insert_with(|| {
                let (_, labels) = self.labels_of_series.entry(key).or_insert_with(|| {
                    let labels = sample
                        .labels
                        .iter()
                        .map(|(name, value)| Label {
                            name: name.clone(),
                            value: value.clone(),
                        })
                        .collect();
                    (Arc::clone(&sample.labels), labels)
                });
                request.timeseries.push(TimeSeries {
                    labels: std::mem::take(labels),
                    samples: Vec::new(),
                });
                keys.push(key);
                request.timeseries.len() - 1
            });
            request.timeseries[position].samples.push(Sample {
                value: sample.value,
                timestamp: sample.ts_ms,
            });
        }
        self.buffer.clear();
        request
            .encode(&mut self.buffer)
            .expect("a Vec grows to fit any message");
        for (series, key) in request.timeseries.into_iter().zip(keys) {
            if let Some((_, labels)) = self.labels_of_series.get_mut(&key) {
                *labels = series.labels;
            }
        }
        self.snappy
            .compress_vec(&self.buffer)
            .expect("snappy compresses any buffer")
    }
}

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub type Labels = BTreeMap<String, String>;

pub struct Asked {
    pub seconds: f64,
    pub outcome: Result<Vec<(Labels, f64)>, String>,
}

fn seconds_text(ms: i64) -> String {
    format!("{}.{:03}", ms.div_euclid(1000), ms.rem_euclid(1000))
}

fn vector(body: &Value) -> Result<Vec<(Labels, f64)>> {
    if body["status"] != "success" {
        bail!("{}", body["error"].as_str().unwrap_or("query failed"));
    }
    if body["data"]["resultType"] != "vector" {
        bail!("result type {} is not a vector", body["data"]["resultType"]);
    }
    let mut out = Vec::new();
    for sample in body["data"]["result"].as_array().into_iter().flatten() {
        let labels: Labels = serde_json::from_value(sample["metric"].clone())?;
        let text = sample["value"][1]
            .as_str()
            .context("a sample has no value")?;
        out.push((labels, text.parse::<f64>()?));
    }
    Ok(out)
}

pub fn ask(agent: &ureq::Agent, base: &str, promql: &str, time_ms: i64) -> Asked {
    ask_with_parameters(agent, base, promql, time_ms, &[])
}

pub fn ask_with_parameters(
    agent: &ureq::Agent,
    base: &str,
    promql: &str,
    time_ms: i64,
    parameters: &[(&str, &str)],
) -> Asked {
    let started = Instant::now();
    let mut request = agent
        .get(&format!("{base}/api/v1/query"))
        .query("query", promql)
        .query("time", &seconds_text(time_ms));
    for (name, value) in parameters {
        request = request.query(name, value);
    }
    let response = request.call();
    let text = match response {
        Ok(response) => response.into_string().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(_, response)) => response.into_string().map_err(|e| e.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let seconds = started.elapsed().as_secs_f64();
    let outcome = text.and_then(|text| {
        serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("{e}: {text}"))
            .and_then(|body| vector(&body).map_err(|e| e.to_string()))
    });
    Asked { seconds, outcome }
}

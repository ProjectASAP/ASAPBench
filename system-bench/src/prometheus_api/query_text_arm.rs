use std::collections::BTreeMap;

use aqpbm_core::harness::data::SampleChunk;
use aqpbm_core::harness::engine::{
    Answers, EngineError, QueryLanguage, SeriesLabels, WorkloadSpec,
};

use super::query::ask_with_parameters;
use super::remote_write::{encode, request_of, send_until_accepted};

pub const BATCH_SAMPLES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryToAsk {
    pub id: String,
    pub text: String,
    pub keeps_metric_name: bool,
}

pub fn queries_to_ask(
    workload: &WorkloadSpec,
    languages: &[QueryLanguage],
) -> Result<Vec<QueryToAsk>, EngineError> {
    workload
        .queries
        .iter()
        .map(|query| {
            languages
                .iter()
                .find_map(|language| query.texts.iter().find(|text| &text.language == language))
                .map(|text| QueryToAsk {
                    id: query.id.clone(),
                    text: text.text.clone(),
                    keeps_metric_name: is_bare_selector(&text.text),
                })
                .ok_or_else(|| {
                    EngineError(format!("query {} has no text in {:?}", query.id, languages))
                })
        })
        .collect()
}

pub fn is_bare_selector(text: &str) -> bool {
    let text = text.trim();
    let (name, matchers) = match text.find('{') {
        Some(open) => (&text[..open], Some(&text[open..])),
        None => (text, None),
    };
    let name = name.trim();
    let name_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    let matchers_ok = matchers.is_none_or(|m| m.ends_with('}') && m.matches('}').count() == 1);
    name_ok && matchers_ok && (!name.is_empty() || matchers.is_some())
}

pub fn write_chunk(
    agent: &ureq::Agent,
    base: &str,
    chunk: &SampleChunk,
) -> Result<(), EngineError> {
    let mut samples = chunk.samples.clone();
    samples.sort_by_key(|sample| sample.ts_ms);
    for batch in samples.chunks(BATCH_SAMPLES) {
        send_until_accepted(agent, base, &encode(&request_of(batch)))
            .map_err(|error| EngineError(format!("{error:#}")))?;
    }
    Ok(())
}

pub fn ask_every_query(
    agent: &ureq::Agent,
    base: &str,
    queries: &[QueryToAsk],
    at_ms: i64,
    parameters: &[(&str, &str)],
) -> Result<Answers, EngineError> {
    let mut answers = BTreeMap::new();
    for query in queries {
        let asked = ask_with_parameters(agent, base, &query.text, at_ms, parameters);
        let samples = asked
            .outcome
            .map_err(|error| EngineError(format!("query {}: {error}", query.id)))?;
        let mut series: BTreeMap<SeriesLabels, f64> = BTreeMap::new();
        for (mut labels, value) in samples {
            if !query.keeps_metric_name {
                labels.remove("__name__");
            }
            if series.insert(labels.clone(), value).is_some() {
                return Err(EngineError(format!(
                    "query {}: two answers for the series {labels:?}",
                    query.id
                )));
            }
        }
        answers.insert(query.id.clone(), series);
    }
    Ok(Answers(answers))
}

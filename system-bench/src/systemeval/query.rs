use std::collections::BTreeMap;

use anyhow::{Context, Result};
use aqpbm_usecase::accuracy::AnswerAt;
use aqpbm_usecase::data::Series;
use aqpbm_usecase::output::{Failure, SystemevalCost, SystemevalOutcome, SystemevalQuery};
use aqpbm_usecase::score::score;
use aqpbm_usecase::spec::QuerySpec;

use crate::prometheus_api::query::Asked;

const KEPT_FAILURES: usize = 5;

#[derive(Default)]
pub struct Collected {
    answered: Vec<AnswerAt>,
    costs: Vec<SystemevalCost>,
    failures: Vec<Failure>,
    failed: usize,
    not_finite: usize,
    times: Vec<i64>,
}

impl Collected {
    pub fn record(&mut self, t: i64, asked: Asked) {
        self.times.push(t);
        let returned = match asked.outcome {
            Ok(samples) => {
                let count = samples.len();
                for (labels, value) in samples {
                    if value.is_finite() {
                        self.answered.push((t, labels, value));
                    } else {
                        self.not_finite += 1;
                    }
                }
                Some(count)
            }
            Err(reason) => {
                self.failed += 1;
                if self.failures.len() < KEPT_FAILURES {
                    self.failures.push(Failure {
                        time: t,
                        error: reason,
                    });
                }
                None
            }
        };
        self.costs.push(SystemevalCost {
            time: t,
            query_seconds: asked.seconds,
            series_returned: returned,
        });
    }

    pub fn finish(
        self,
        query: &QuerySpec,
        series: &[Series],
        exact: Option<&BTreeMap<String, Vec<AnswerAt>>>,
    ) -> Result<SystemevalQuery> {
        let outcome = if self.failed == self.times.len() {
            SystemevalOutcome::Unsupported {
                unsupported: self
                    .failures
                    .first()
                    .map_or_else(|| "no evaluation answered".to_string(), |f| f.error.clone()),
            }
        } else {
            let scored = score(query, series, &self.times, &self.answered, exact)
                .with_context(|| format!("scoring {}", query.id))?;
            SystemevalOutcome::Scored {
                answers: scored.answers,
                computed_truth: scored.computed_truth,
            }
        };
        Ok(SystemevalQuery {
            id: query.id.clone(),
            promql: query.promql.clone(),
            unit: query.unit.clone(),
            threshold: query.threshold,
            evaluations: self.times.len(),
            failed_evaluations: self.failed,
            failures: self.failures,
            not_finite_answers: self.not_finite,
            costs: self.costs,
            outcome,
        })
    }
}

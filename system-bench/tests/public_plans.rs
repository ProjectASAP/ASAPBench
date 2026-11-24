//! An equivalent-only plan must translate (the capability README's
//! rule). Every planner-written plan of a public synthetic workload under
//! `fixtures/plans/public/` translates, and the equivalent-only ones with no
//! substitution. A query-time node several readers share is recomputed per
//! reader (`SharingLost`); that changes cost, not answers, so it is no
//! substitution.
//!
//! The committed plans are the showcase shapes plus a cover of every
//! node shape that planning all 206 public PromQL workloads with the two
//! ClickHouse files produces (see `fixtures/plans/REV`).

mod common;

use system_bench::engines_external::clickhouse::translate;

/// `(workload, variant)` of every committed public plan, in name order.
fn public_plans() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plans/public");
    let mut plans: Vec<(String, String)> = std::fs::read_dir(dir)
        .expect("public plans")
        .map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let stem = name.strip_suffix(".plan.json").expect("a plan file");
            let (workload, variant) = stem
                .split_once(".clickhouse.")
                .expect("<workload>.clickhouse.<variant>");
            (workload.to_string(), format!("clickhouse.{variant}"))
        })
        .collect();
    plans.sort();
    plans
}

#[test]
fn public_plans_translate() {
    let plans = public_plans();
    // 18 equivalent-only plans and 11 with substitutes.
    assert_eq!(plans.len(), 29, "{plans:?}");
    let mut failures = Vec::new();
    for (workload, variant) in &plans {
        let plan = common::public_fixture(workload, variant);
        match translate(&plan) {
            Err(error) => failures.push(format!("{workload} {variant}: {error}")),
            Ok(deployment) => {
                if variant == "clickhouse.equivalent-only" && !deployment.substitutions.is_empty() {
                    failures.push(format!(
                        "{workload} {variant}: substitutions {:?}",
                        deployment.substitutions
                    ));
                }
                assert_eq!(deployment.queries.len(), plan.roots.len());
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

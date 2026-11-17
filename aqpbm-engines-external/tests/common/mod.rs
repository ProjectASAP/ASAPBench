//! Shared helpers: load a planner-written fixture, change one field, and
//! compare SQL ignoring runs of whitespace.
#![allow(dead_code)]

use std::path::PathBuf;

use aqpbm_engines_external::plan_file::PlanFile;
use serde_json::Value;

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plans")
        .join(format!("{name}.plan.json"))
}

/// The fixture as JSON, to be changed by a mutation.
pub fn fixture_value(name: &str) -> Value {
    let text = std::fs::read_to_string(fixture_path(name)).expect("fixture");
    serde_json::from_str(&text).expect("fixture JSON")
}

/// The fixture decoded.
pub fn fixture(name: &str) -> PlanFile {
    PlanFile::from_value(fixture_value(name)).expect("fixture decodes")
}

/// The payload body of node `id` (`payload.<side>.<operator>`), to change.
pub fn payload_body(plan: &mut Value, id: usize) -> &mut Value {
    let payload = &mut plan["dag"]["nodes"][id]["payload"];
    let side = payload.as_object_mut().unwrap().values_mut().next().unwrap();
    side.as_object_mut().unwrap().values_mut().next().unwrap()
}

/// SQL without `--` comment lines, with every run of whitespace replaced by
/// one space.
pub fn normalized(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/clickhouse")
        .join(name);
    std::fs::read_to_string(path).expect("golden")
}

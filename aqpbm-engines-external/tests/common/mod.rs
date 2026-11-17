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
    let side = payload
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
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

/// A planner-written fixture of a public workload under
/// `fixtures/plans/public/`, e.g. `("q4c_sum_over_time__1m_1hz_10s",
/// "clickhouse.equivalent-only")`.
pub fn public_fixture(workload: &str, variant: &str) -> PlanFile {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plans/public")
        .join(format!("{workload}.{variant}.plan.json"));
    let text = std::fs::read_to_string(path).expect("public fixture");
    PlanFile::from_json(&text).expect("public fixture decodes")
}

/// A public fixture as JSON, to be changed by a mutation.
pub fn fixture_value_public(workload: &str, variant: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plans/public")
        .join(format!("{workload}.{variant}.plan.json"));
    serde_json::from_str(&std::fs::read_to_string(path).expect("public fixture"))
        .expect("public fixture JSON")
}

/// The `clickhouse local` command of `AQPBM_CLICKHOUSE_LOCAL`: a shell
/// command that runs ClickHouse 26.8.39.13 reading SQL on stdin.
pub fn clickhouse_local() -> String {
    std::env::var("AQPBM_CLICKHOUSE_LOCAL").expect("AQPBM_CLICKHOUSE_LOCAL")
}

/// Runs `script` with the query parameter `t`; the TSV output rows, sorted
/// and joined by newlines, or ClickHouse's error output.
pub fn run_clickhouse(script: &str, t: i64) -> Result<String, String> {
    use std::io::Write;
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{} --param_t={t}", clickhouse_local()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("clickhouse local");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let mut rows: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    rows.sort_unstable();
    Ok(rows.join("\n"))
}

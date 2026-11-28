use std::process::Command;
use std::time::{Duration, Instant};

use aqpbm_core::harness::engine::EngineError;

pub(crate) fn restart_and_wait(
    engine: &str,
    command: Option<&str>,
    ready_timeout: Duration,
    mut ready: impl FnMut() -> bool,
) -> Result<(), EngineError> {
    let Some(command) = command else {
        return Err(EngineError(format!(
            "a cold run restarts the {engine} server; give its restart command"
        )));
    };
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()
        .map_err(|error| EngineError(format!("restart command: {error}")))?;
    if !status.success() {
        return Err(EngineError(format!("restart command exited with {status}")));
    }
    let start = Instant::now();
    loop {
        if ready() {
            return Ok(());
        }
        if start.elapsed() > ready_timeout {
            return Err(EngineError(format!(
                "{engine} did not answer within {ready_timeout:?} of its restart"
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

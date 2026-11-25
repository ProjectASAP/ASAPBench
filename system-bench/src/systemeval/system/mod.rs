mod asapquery_backend;
mod victoriametrics;

use anyhow::Result;
use serde_json::Value;

pub use asapquery_backend::AsapqueryBackend;
pub use victoriametrics::Victoriametrics;

pub use crate::prometheus_api::remote_write::Sent;

pub trait System {
    fn id(&self) -> &'static str;

    fn name(&self) -> &'static str;

    fn write(&self, agent: &ureq::Agent, base: &str, body: &[u8]) -> Result<Sent> {
        crate::prometheus_api::remote_write::send(agent, base, body)
    }

    fn make_visible(&self, agent: &ureq::Agent, base: &str, through_ms: i64) -> Result<()>;

    fn status(&self, _agent: &ureq::Agent, _base: &str) -> Result<Option<Value>> {
        Ok(None)
    }
}

pub const SYSTEMS: &[&dyn System] = &[&AsapqueryBackend, &Victoriametrics];

pub fn ids() -> Vec<&'static str> {
    SYSTEMS.iter().map(|system| system.id()).collect()
}

pub fn by_id(id: &str) -> Option<&'static dyn System> {
    SYSTEMS.iter().copied().find(|system| system.id() == id)
}

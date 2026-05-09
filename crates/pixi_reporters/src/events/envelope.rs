use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::event::Event;

/// One line of the JSONL stream.
///
/// `seq` is assigned by the sink (monotonic in write order). `ts_unix_ns`
/// is captured at producer side via [`now_unix_ns`] just before handing the
/// envelope off to the sink.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub seq: u64,
    pub ts_unix_ns: u64,
    #[serde(flatten)]
    pub event: Event,
}

impl EventEnvelope {
    /// Build an envelope with `seq` set to 0; the sink overwrites it.
    pub fn new(event: Event) -> Self {
        Self {
            seq: 0,
            ts_unix_ns: now_unix_ns(),
            event,
        }
    }
}

pub fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

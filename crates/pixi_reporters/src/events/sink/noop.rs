use crate::events::{EventEnvelope, sink::EventSink};

/// Discards every event. Used as the default when the env var is unset
/// or the sink couldn't be constructed.
pub struct NoopSink;

impl EventSink for NoopSink {
    fn record(&self, _: EventEnvelope) {}
}

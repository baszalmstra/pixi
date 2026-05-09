//! Event sinks: a small trait + concrete implementations.

mod jsonl;
mod noop;

use std::sync::Arc;

pub use jsonl::JsonlSink;
pub use noop::NoopSink;

use crate::events::EventEnvelope;

/// Receives [`EventEnvelope`]s from the streaming reporters. Implementations
/// are expected to be cheap on the producer side (e.g. forward to a
/// background writer thread).
pub trait EventSink: Send + Sync {
    fn record(&self, envelope: EventEnvelope);
}

/// Convenience handle for sharing a sink across reporter instances.
pub type SinkHandle = Arc<dyn EventSink>;

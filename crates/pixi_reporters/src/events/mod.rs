//! Typed progress-event stream.
//!
//! Decoupled from indicatif: every reporter callback can be turned into an
//! `Event` and routed through an `EventSink` (e.g. a JSONL file). The
//! visualization (rich CLI, Python prototype, agents) reads the stream.

use std::sync::{Arc, OnceLock};

pub mod envelope;
pub mod event;
pub mod sink;
pub mod streaming;
pub mod streaming_gateway;
pub mod streaming_rattler_install;
pub mod tracing_layer;

pub use envelope::EventEnvelope;
pub use event::*;
pub use sink::{EventSink, JsonlSink, NoopSink, SinkHandle};
pub use streaming::*;
pub use streaming_gateway::{StreamingGatewayDownloadReporter, StreamingGatewayReporter};
pub use streaming_rattler_install::StreamingRattlerInstallReporter;
pub use tracing_layer::EventStreamLayer;

/// Environment variable consulted by [`shared_sink`] and
/// [`event_stream_enabled`]. When set to a non-empty path, events are
/// recorded as JSONL alongside normal progress reporting.
pub const EVENT_STREAM_ENV_VAR: &str = "PIXI_EVENT_STREAM";

static SHARED_SINK: OnceLock<SinkHandle> = OnceLock::new();

/// Returns `true` iff [`EVENT_STREAM_ENV_VAR`] is set to a non-empty
/// value at the moment of the call.
pub fn event_stream_enabled() -> bool {
    std::env::var(EVENT_STREAM_ENV_VAR)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Lazy-initialize and return the process-wide event sink. The first
/// caller decides whether the sink writes to a JSONL file (when
/// [`EVENT_STREAM_ENV_VAR`] points at a writable path) or discards
/// events (`NoopSink`). All subsequent callers see the same `Arc`.
pub fn shared_sink() -> SinkHandle {
    SHARED_SINK.get_or_init(build_sink_from_env).clone()
}

/// Returns an [`EventStreamLayer`] backed by [`shared_sink`] iff the
/// env var is set; otherwise `None`. Compose into a
/// `tracing_subscriber::Registry` to bridge tracing events into the
/// stream as `Event::Log` entries.
pub fn event_stream_layer() -> Option<EventStreamLayer> {
    event_stream_enabled().then(|| EventStreamLayer::new(shared_sink()))
}

fn build_sink_from_env() -> SinkHandle {
    match std::env::var(EVENT_STREAM_ENV_VAR) {
        Ok(path) if !path.is_empty() => match JsonlSink::new(&path) {
            Ok(sink) => Arc::new(sink) as SinkHandle,
            Err(e) => {
                tracing::warn!(
                    target: "pixi::events",
                    error = %e,
                    path = %path,
                    "failed to open PIXI_EVENT_STREAM file; events disabled",
                );
                Arc::new(NoopSink) as SinkHandle
            }
        },
        _ => Arc::new(NoopSink) as SinkHandle,
    }
}

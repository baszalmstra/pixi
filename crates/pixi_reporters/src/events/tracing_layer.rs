//! `tracing_subscriber::Layer` that bridges `tracing` events into the
//! event stream as [`Event::Log`].

use std::fmt;

use pixi_compute_reporters::OperationId;
use serde_json::Value;
use tracing::{Event as TracingEvent, Subscriber, field::Field};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

use crate::events::{
    Event, EventEnvelope, LogEntry, LogLevel,
    sink::SinkHandle,
};

/// Layer that emits an [`Event::Log`] into `sink` for every `tracing`
/// event seen.
pub struct EventStreamLayer {
    sink: SinkHandle,
}

impl EventStreamLayer {
    pub fn new(sink: SinkHandle) -> Self {
        Self { sink }
    }
}

impl<S> Layer<S> for EventStreamLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &TracingEvent<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);

        let entry = LogEntry {
            id: OperationId::current(),
            level: tracing_level(metadata.level()),
            target: metadata.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        self.sink
            .record(EventEnvelope::new(Event::Log(entry)));
    }
}

fn tracing_level(level: &tracing::Level) -> LogLevel {
    match *level {
        tracing::Level::TRACE => LogLevel::Trace,
        tracing::Level::DEBUG => LogLevel::Debug,
        tracing::Level::INFO => LogLevel::Info,
        tracing::Level::WARN => LogLevel::Warn,
        tracing::Level::ERROR => LogLevel::Error,
    }
}

#[derive(Default)]
struct LogVisitor {
    message: String,
    fields: serde_json::Map<String, Value>,
}

impl tracing::field::Visit for LogVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let formatted = format!("{value:?}");
        if field.name() == "message" {
            self.message = formatted;
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(formatted));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::events::{Event, EventEnvelope, sink::EventSink};

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<EventEnvelope>>);

    impl EventSink for CapturingSink {
        fn record(&self, envelope: EventEnvelope) {
            self.0.lock().unwrap().push(envelope);
        }
    }

    #[test]
    fn captures_tracing_event_as_log() {
        let sink = Arc::new(CapturingSink::default());
        let layer = EventStreamLayer::new(sink.clone());
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "pixi::test", count = 3, "hello {}", "world");
        });

        let captured = sink.0.lock().unwrap();
        assert_eq!(captured.len(), 1);
        match &captured[0].event {
            Event::Log(entry) => {
                assert_eq!(entry.target, "pixi::test");
                assert!(matches!(entry.level, LogLevel::Info));
                assert_eq!(entry.message, "hello world");
                assert_eq!(
                    entry.fields.get("count").and_then(|v| v.as_i64()),
                    Some(3)
                );
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }
}

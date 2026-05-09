use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::JoinHandle,
};

use crate::events::{
    EventEnvelope,
    envelope::now_unix_ns,
    event::{Event, SCHEMA_VERSION},
    sink::EventSink,
};

const CHANNEL_CAPACITY: usize = 4096;
const WRITER_THREAD_NAME: &str = "pixi-event-stream-writer";

/// JSONL file sink. One event per line. A dedicated writer thread
/// assigns the monotonic `seq`, serializes, appends, and flushes.
///
/// The producer side is non-blocking; on overflow events are dropped
/// and counted in `dropped`.
pub struct JsonlSink {
    tx: Mutex<Option<SyncSender<EventEnvelope>>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    dropped: AtomicU64,
}

impl JsonlSink {
    /// Open `path` for writing and start the background writer.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = File::create(path.as_ref())?;
        let writer = BufWriter::new(file);
        let (tx, rx) = mpsc::sync_channel::<EventEnvelope>(CHANNEL_CAPACITY);

        let handle = std::thread::Builder::new()
            .name(WRITER_THREAD_NAME.into())
            .spawn(move || writer_loop(rx, writer))?;

        let sink = Self {
            tx: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
            dropped: AtomicU64::new(0),
        };
        sink.record(EventEnvelope {
            seq: 0,
            ts_unix_ns: now_unix_ns(),
            event: Event::StreamHeader {
                schema_version: SCHEMA_VERSION,
                started_at_unix_ns: now_unix_ns(),
                pixi_version: env!("CARGO_PKG_VERSION").into(),
            },
        });
        Ok(sink)
    }

    /// Number of envelopes that were dropped because the writer queue
    /// was full when the producer tried to enqueue.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Close the channel and wait for the writer to drain. Idempotent.
    pub fn close(&self) {
        if let Ok(mut tx_slot) = self.tx.lock() {
            tx_slot.take();
        }
        if let Ok(mut handle_slot) = self.handle.lock() {
            if let Some(h) = handle_slot.take() {
                let _ = h.join();
            }
        }
    }
}

impl EventSink for JsonlSink {
    fn record(&self, envelope: EventEnvelope) {
        let Ok(tx_slot) = self.tx.lock() else { return };
        let Some(tx) = tx_slot.as_ref() else { return };
        match tx.try_send(envelope) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for JsonlSink {
    fn drop(&mut self) {
        self.close();
    }
}

fn writer_loop(rx: mpsc::Receiver<EventEnvelope>, mut writer: BufWriter<File>) {
    let mut seq: u64 = 0;
    while let Ok(mut env) = rx.recv() {
        env.seq = seq;
        seq += 1;
        match serde_json::to_string(&env) {
            Ok(line) => {
                if writer.write_all(line.as_bytes()).is_err()
                    || writer.write_all(b"\n").is_err()
                {
                    return;
                }
                let _ = writer.flush();
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize event envelope");
            }
        }
    }
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{BufRead, BufReader},
    };

    use tempfile::tempdir;

    use super::*;
    use crate::events::{Event, EventEnvelope};

    fn read_lines(path: &Path) -> Vec<String> {
        let f = File::open(path).unwrap();
        BufReader::new(f).lines().filter_map(Result::ok).collect()
    }

    #[test]
    fn writes_jsonl_in_order_with_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("stream.jsonl");

        {
            let sink = JsonlSink::new(&path).unwrap();
            sink.record(EventEnvelope::new(Event::PixiSolveQueued {
                id: pixi_compute_reporters::OperationId(1),
                parent: None,
                environment: "default".into(),
                platform: "linux-64".into(),
                has_direct_conda_dependency: false,
            }));
            sink.record(EventEnvelope::new(Event::PixiSolveStarted {
                id: pixi_compute_reporters::OperationId(1),
            }));
            sink.record(EventEnvelope::new(Event::PixiSolveFinished {
                id: pixi_compute_reporters::OperationId(1),
            }));
        } // drop joins the writer

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 4, "header + 3 events");

        let parsed: Vec<EventEnvelope> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(matches!(parsed[0].event, Event::StreamHeader { .. }));
        assert_eq!(parsed[0].seq, 0);
        assert_eq!(parsed[1].seq, 1);
        assert_eq!(parsed[2].seq, 2);
        assert_eq!(parsed[3].seq, 3);
        assert!(matches!(parsed[1].event, Event::PixiSolveQueued { .. }));
        assert!(matches!(parsed[2].event, Event::PixiSolveStarted { .. }));
        assert!(matches!(parsed[3].event, Event::PixiSolveFinished { .. }));
    }

    #[test]
    fn close_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("stream.jsonl");
        let sink = JsonlSink::new(&path).unwrap();
        sink.close();
        sink.close();
    }
}

//! Streaming decorators for [`GatewayReporter`] and the rattler-side
//! [`rattler_repodata_gateway::Reporter`] / [`DownloadReporter`] it
//! produces.
//!
//! `StreamingGatewayReporter` wraps an inner pixi-side reporter and
//! emits `GatewayQueryQueued/Started/Finished` events. Its
//! `on_queued` builds a `StreamingGatewayDownloadReporter` for the
//! rattler side, parented to the freshly-allocated query id.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use pixi_command_dispatcher::{GatewayQuerySpec, GatewayReporter};
use pixi_compute_reporters::{OperationId, OperationRegistry};
use rattler_repodata_gateway::{DownloadReporter, Reporter as RattlerGatewayReporter};
use url::Url;

use crate::events::{Event, EventEnvelope, sink::SinkHandle};

fn emit(sink: &SinkHandle, event: Event) {
    sink.record(EventEnvelope::new(event));
}

pub struct StreamingGatewayReporter {
    pub inner: Arc<dyn GatewayReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl GatewayReporter for StreamingGatewayReporter {
    fn on_queued(
        &self,
        spec: &GatewayQuerySpec,
    ) -> (OperationId, Option<Box<dyn RattlerGatewayReporter>>) {
        let (id, inner_rattler) = self.inner.on_queued(spec);
        let parent = self.registry.parent_of(id);
        emit(
            &self.sink,
            Event::GatewayQueryQueued {
                id,
                parent,
                channels: spec.channels.iter().map(|c| c.to_string()).collect(),
                platforms: spec.platforms.iter().map(|p| p.to_string()).collect(),
            },
        );
        let streamed: Box<dyn RattlerGatewayReporter> =
            Box::new(StreamingGatewayDownloadReporter::new(
                inner_rattler,
                self.sink.clone(),
                self.registry.clone(),
                id,
            ));
        (id, Some(streamed))
    }

    fn on_started(&self, query_id: OperationId) {
        self.inner.on_started(query_id);
        emit(&self.sink, Event::GatewayQueryStarted { id: query_id });
    }

    fn on_finished(&self, query_id: OperationId) {
        self.inner.on_finished(query_id);
        emit(&self.sink, Event::GatewayQueryFinished { id: query_id });
    }
}

/// Implements rattler's `Reporter` and `DownloadReporter`. The gateway
/// passes a `usize` index per download; we map those to the
/// [`OperationId`]s allocated for the streaming events. Both rattler
/// traits take `&self`, so internal mutable state lives behind a
/// [`Mutex`].
pub struct StreamingGatewayDownloadReporter {
    inner: Option<Box<dyn RattlerGatewayReporter>>,
    sink: SinkHandle,
    registry: Arc<OperationRegistry>,
    parent_query_id: OperationId,
    indexes: Mutex<HashMap<usize, OperationId>>,
}

impl StreamingGatewayDownloadReporter {
    pub fn new(
        inner: Option<Box<dyn RattlerGatewayReporter>>,
        sink: SinkHandle,
        registry: Arc<OperationRegistry>,
        parent_query_id: OperationId,
    ) -> Self {
        Self {
            inner,
            sink,
            registry,
            parent_query_id,
            indexes: Mutex::new(HashMap::new()),
        }
    }
}

impl RattlerGatewayReporter for StreamingGatewayDownloadReporter {
    fn download_reporter(&self) -> Option<&dyn DownloadReporter> {
        Some(self)
    }
}

impl DownloadReporter for StreamingGatewayDownloadReporter {
    fn on_download_start(&self, url: &Url) -> usize {
        let inner_idx = self
            .inner
            .as_ref()
            .and_then(|r| r.download_reporter())
            .map(|d| d.on_download_start(url))
            .unwrap_or_else(|| {
                // No inner reporter; mint a stable index from the map
                // size so the gateway's own index space is unambiguous.
                self.indexes.lock().unwrap().len()
            });
        let id = self.registry.allocate();
        self.indexes.lock().unwrap().insert(inner_idx, id);
        emit(
            &self.sink,
            Event::RepodataDownloadStarted {
                id,
                parent: self.parent_query_id,
                url: url.to_string(),
            },
        );
        inner_idx
    }

    fn on_download_progress(
        &self,
        url: &Url,
        index: usize,
        bytes_downloaded: usize,
        total_bytes: Option<usize>,
    ) {
        if let Some(inner) = self.inner.as_ref().and_then(|r| r.download_reporter()) {
            inner.on_download_progress(url, index, bytes_downloaded, total_bytes);
        }
        if let Some(&id) = self.indexes.lock().unwrap().get(&index) {
            emit(
                &self.sink,
                Event::RepodataDownloadProgress {
                    id,
                    bytes: bytes_downloaded as u64,
                    total: total_bytes.map(|t| t as u64),
                },
            );
        }
    }

    fn on_download_complete(&self, url: &Url, index: usize) {
        if let Some(inner) = self.inner.as_ref().and_then(|r| r.download_reporter()) {
            inner.on_download_complete(url, index);
        }
        if let Some(id) = self.indexes.lock().unwrap().remove(&index) {
            emit(&self.sink, Event::RepodataDownloadComplete { id });
        }
    }
}

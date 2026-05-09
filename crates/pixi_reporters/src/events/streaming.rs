//! Streaming decorators for the per-area pixi reporter traits.
//!
//! Each decorator wraps an inner reporter (as `Arc<dyn TraitName>`) and an
//! [`EventSink`]. Lifecycle hooks forward to the inner reporter and then
//! emit the matching [`Event`] variant. The `OperationId` returned by the
//! inner is the canonical id; the decorator looks the parent up via
//! [`OperationRegistry::parent_of`] so the event matches what every other
//! consumer of the registry sees.

use std::sync::Arc;

use futures::{Stream, StreamExt};
use pixi_build_discovery::JsonRpcBackendSpec;
use pixi_command_dispatcher::{
    BackendSourceBuildReporter, BackendSourceBuildSpec, BuildBackendMetadataInner,
    BuildBackendMetadataReporter, CondaSolveReporter, GitCheckoutReporter,
    InstallPixiEnvironmentSpec, InstantiateBackendReporter, PixiInstallReporter,
    PixiSolveReporter, SolveCondaEnvironmentSpec, SourceMetadataReporter,
    SourceMetadataReporterSpec, SourceRecordReporter, SourceRecordReporterSpec,
    UrlCheckoutReporter,
};
use pixi_compute_reporters::{OperationId, OperationRegistry};
use pixi_git::resolver::RepositoryReference;
use url::Url;

use crate::events::{BackendStream, Event, EventEnvelope, sink::SinkHandle};

fn emit(sink: &SinkHandle, event: Event) {
    sink.record(EventEnvelope::new(event));
}

fn parent(registry: &OperationRegistry, id: OperationId) -> Option<OperationId> {
    registry.parent_of(id)
}

/// Wrap a stream of backend-output lines so each item is also emitted as
/// an [`Event::BackendOutput`] tagged with `id` before continuing
/// downstream.
fn tee_backend_output(
    sink: SinkHandle,
    id: OperationId,
    inner: Box<dyn Stream<Item = String> + Unpin + Send>,
) -> Box<dyn Stream<Item = String> + Unpin + Send> {
    let teed = inner.inspect(move |line| {
        emit(
            &sink,
            Event::BackendOutput {
                id,
                stream: BackendStream::Stdout,
                line: line.clone(),
            },
        );
    });
    Box::new(Box::pin(teed))
}

// =============== PixiSolve ===============

pub struct StreamingPixiSolveReporter {
    pub inner: Arc<dyn PixiSolveReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl PixiSolveReporter for StreamingPixiSolveReporter {
    fn on_queued(
        &self,
        env: &pixi_command_dispatcher::PixiSolveEnvironmentSpec,
    ) -> OperationId {
        let id = self.inner.on_queued(env);
        emit(
            &self.sink,
            Event::PixiSolveQueued {
                id,
                parent: parent(&self.registry, id),
                environment: env.name.clone(),
                platform: env.platform.to_string(),
                has_direct_conda_dependency: env.has_direct_conda_dependency,
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::PixiSolveStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::PixiSolveFinished { id });
    }
}

// =============== CondaSolve ===============

pub struct StreamingCondaSolveReporter {
    pub inner: Arc<dyn CondaSolveReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl CondaSolveReporter for StreamingCondaSolveReporter {
    fn on_queued(&self, env: &SolveCondaEnvironmentSpec) -> OperationId {
        let id = self.inner.on_queued(env);
        emit(
            &self.sink,
            Event::CondaSolveQueued {
                id,
                parent: parent(&self.registry, id),
                environment: env.name.clone(),
                platform: env.platform.to_string(),
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::CondaSolveStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::CondaSolveFinished { id });
    }
}

// =============== PixiInstall ===============

pub struct StreamingPixiInstallReporter {
    pub inner: Arc<dyn PixiInstallReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl PixiInstallReporter for StreamingPixiInstallReporter {
    fn on_queued(&self, env: &InstallPixiEnvironmentSpec) -> OperationId {
        let id = self.inner.on_queued(env);
        emit(
            &self.sink,
            Event::PixiInstallQueued {
                id,
                parent: parent(&self.registry, id),
                environment: env.name.clone(),
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::PixiInstallStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::PixiInstallFinished { id });
    }

    fn create_install_reporter(&self) -> Option<Box<dyn rattler::install::Reporter>> {
        let inner = self.inner.create_install_reporter()?;
        let install_id = OperationId::current()?;
        Some(Box::new(super::streaming_rattler_install::StreamingRattlerInstallReporter::new(
            inner,
            self.sink.clone(),
            self.registry.clone(),
            install_id,
        )))
    }
}

// =============== GitCheckout ===============

pub struct StreamingGitCheckoutReporter {
    pub inner: Arc<dyn GitCheckoutReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl GitCheckoutReporter for StreamingGitCheckoutReporter {
    fn on_queued(&self, repo: &RepositoryReference) -> OperationId {
        let id = self.inner.on_queued(repo);
        emit(
            &self.sink,
            Event::GitCheckoutQueued {
                id,
                parent: parent(&self.registry, id),
                url: repo.url.as_url().to_string(),
                reference: format!("{:?}", repo.reference),
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::GitCheckoutStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::GitCheckoutFinished { id });
    }
}

// =============== UrlCheckout ===============

pub struct StreamingUrlCheckoutReporter {
    pub inner: Arc<dyn UrlCheckoutReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl UrlCheckoutReporter for StreamingUrlCheckoutReporter {
    fn on_queued(&self, url: &Url) -> OperationId {
        let id = self.inner.on_queued(url);
        emit(
            &self.sink,
            Event::UrlCheckoutQueued {
                id,
                parent: parent(&self.registry, id),
                url: url.to_string(),
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::UrlCheckoutStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::UrlCheckoutFinished { id });
    }
}

// =============== InstantiateBackend ===============

pub struct StreamingInstantiateBackendReporter {
    pub inner: Arc<dyn InstantiateBackendReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl InstantiateBackendReporter for StreamingInstantiateBackendReporter {
    fn on_queued(&self, spec: &JsonRpcBackendSpec) -> OperationId {
        let id = self.inner.on_queued(spec);
        emit(
            &self.sink,
            Event::InstantiateBackendQueued {
                id,
                parent: parent(&self.registry, id),
                backend: spec.name.clone(),
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::InstantiateBackendStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::InstantiateBackendFinished { id });
    }

    fn create_install_reporter(&self) -> Option<Box<dyn rattler::install::Reporter>> {
        let inner = self.inner.create_install_reporter()?;
        let install_id = OperationId::current()?;
        Some(Box::new(super::streaming_rattler_install::StreamingRattlerInstallReporter::new(
            inner,
            self.sink.clone(),
            self.registry.clone(),
            install_id,
        )))
    }
}

// =============== BuildBackendMetadata ===============

pub struct StreamingBuildBackendMetadataReporter {
    pub inner: Arc<dyn BuildBackendMetadataReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl BuildBackendMetadataReporter for StreamingBuildBackendMetadataReporter {
    fn on_queued(&self, env: &BuildBackendMetadataInner) -> OperationId {
        let id = self.inner.on_queued(env);
        emit(
            &self.sink,
            Event::BuildBackendMetadataQueued {
                id,
                parent: parent(&self.registry, id),
                manifest_source: format!("{:?}", env.manifest_source),
            },
        );
        id
    }

    fn on_started(
        &self,
        id: OperationId,
        backend_output_stream: Box<dyn Stream<Item = String> + Unpin + Send>,
    ) {
        let teed = tee_backend_output(self.sink.clone(), id, backend_output_stream);
        self.inner.on_started(id, teed);
        emit(&self.sink, Event::BuildBackendMetadataStarted { id });
    }

    fn on_finished(&self, id: OperationId, failed: bool) {
        self.inner.on_finished(id, failed);
        emit(
            &self.sink,
            Event::BuildBackendMetadataFinished { id, failed },
        );
    }
}

// =============== SourceMetadata ===============

pub struct StreamingSourceMetadataReporter {
    pub inner: Arc<dyn SourceMetadataReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl SourceMetadataReporter for StreamingSourceMetadataReporter {
    fn on_queued(&self, spec: &SourceMetadataReporterSpec) -> OperationId {
        let id = self.inner.on_queued(spec);
        emit(
            &self.sink,
            Event::SourceMetadataQueued {
                id,
                parent: parent(&self.registry, id),
                package: spec.package.as_normalized().to_string(),
                backend: format!("{:?}", spec.backend_metadata.manifest_source),
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::SourceMetadataStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::SourceMetadataFinished { id });
    }
}

// =============== SourceRecord ===============

pub struct StreamingSourceRecordReporter {
    pub inner: Arc<dyn SourceRecordReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl SourceRecordReporter for StreamingSourceRecordReporter {
    fn on_queued(&self, spec: &SourceRecordReporterSpec) -> OperationId {
        let id = self.inner.on_queued(spec);
        let variant: Vec<(String, String)> = spec
            .variants
            .iter()
            .map(|(k, v)| (k.clone(), format!("{v:?}")))
            .collect();
        emit(
            &self.sink,
            Event::SourceRecordQueued {
                id,
                parent: parent(&self.registry, id),
                package: spec.package.as_normalized().to_string(),
                variant,
            },
        );
        id
    }

    fn on_started(&self, id: OperationId) {
        self.inner.on_started(id);
        emit(&self.sink, Event::SourceRecordStarted { id });
    }

    fn on_finished(&self, id: OperationId) {
        self.inner.on_finished(id);
        emit(&self.sink, Event::SourceRecordFinished { id });
    }
}

// =============== BackendSourceBuild ===============

pub struct StreamingBackendSourceBuildReporter {
    pub inner: Arc<dyn BackendSourceBuildReporter>,
    pub sink: SinkHandle,
    pub registry: Arc<OperationRegistry>,
}

impl BackendSourceBuildReporter for StreamingBackendSourceBuildReporter {
    fn on_queued(&self, env: &BackendSourceBuildSpec) -> OperationId {
        let id = self.inner.on_queued(env);
        emit(
            &self.sink,
            Event::BackendSourceBuildQueued {
                id,
                parent: parent(&self.registry, id),
                package: env.name.as_normalized().to_string(),
                version: env.version.to_string(),
                build: env.build.clone(),
                subdir: env.subdir.clone(),
            },
        );
        id
    }

    fn on_started(
        &self,
        id: OperationId,
        backend_output_stream: Box<dyn Stream<Item = String> + Unpin + Send>,
    ) {
        let teed = tee_backend_output(self.sink.clone(), id, backend_output_stream);
        self.inner.on_started(id, teed);
        emit(&self.sink, Event::BackendSourceBuildStarted { id });
    }

    fn on_finished(&self, id: OperationId, failed: bool) {
        self.inner.on_finished(id, failed);
        emit(&self.sink, Event::BackendSourceBuildFinished { id, failed });
    }
}

mod download_verify_reporter;
pub mod events;
mod git;
mod main_progress_bar;
mod release_notes;
mod repodata_reporter;
mod sync_reporter;
pub mod uv_reporter;

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

use events::{
    NoopSink, SinkHandle, StreamingBackendSourceBuildReporter,
    StreamingCondaSolveReporter, StreamingGatewayReporter,
    StreamingInstantiateBackendReporter, StreamingPixiInstallReporter,
    StreamingPixiSolveReporter,
};
use git::GitCheckoutProgress;
use indicatif::{MultiProgress, ProgressBar};
use main_progress_bar::MainProgressBar;
use parking_lot::Mutex;
use pixi_command_dispatcher::{
    CommandDispatcherBuilder, GatewayQuerySpec, InstallPixiEnvironmentSpec,
    PixiSolveEnvironmentSpec, SolveCondaEnvironmentSpec,
};
use pixi_compute_reporters::{OperationId, OperationRegistry};
pub use release_notes::format_release_notes;
use repodata_reporter::RepodataReporter;
use sync_reporter::SyncReporter;
use uv_configuration::RAYON_INITIALIZE;
// Re-export the uv_reporter types for external use
pub use uv_reporter::{UvReporter, UvReporterOptions};

// Re-export for back-compat at the crate root.
pub use events::EVENT_STREAM_ENV_VAR;

/// Top-level progress reporter for `pixi`'s CLI. Use
/// [`Self::register_with`] to wire it into a [`CommandDispatcherBuilder`];
/// keep the `Arc` around to call [`Self::on_clear`] between phases.
pub struct TopLevelProgress {
    registry: Arc<OperationRegistry>,
    source_checkout_reporter: Arc<GitCheckoutProgress>,
    conda_solve_reporter: MainProgressBar<String>,
    /// `OperationId` → bar slot in `conda_solve_reporter`. Lets
    /// `on_started` / `on_finished` find the bar created at `on_queued`.
    solve_bars: Mutex<HashMap<OperationId, usize>>,
    // Held so on_clear can wipe it; the gateway integration that would
    // populate it currently goes through a different code path, so the
    // bar is effectively a placeholder for future wiring.
    repodata_reporter: RepodataReporter,
    sync_reporter: SyncReporter,
    event_sink: SinkHandle,
}

impl TopLevelProgress {
    /// Build an `Arc<Self>` anchored on the global multi-progress with
    /// a fresh [`OperationRegistry`]. Reads [`EVENT_STREAM_ENV_VAR`] to
    /// decide whether to enable JSONL event recording.
    pub fn from_global() -> Arc<Self> {
        let multi_progress = pixi_progress::global_multi_progress();
        let anchor_pb = multi_progress.add(ProgressBar::hidden());
        Arc::new(Self::new_with_sink(
            OperationRegistry::new(),
            multi_progress,
            anchor_pb,
            events::shared_sink(),
        ))
    }

    /// The registry used by this progress reporter to allocate ids and
    /// look up parent relationships.
    pub fn registry(&self) -> &Arc<OperationRegistry> {
        &self.registry
    }

    /// Construct a new top level progress reporter. All progress bars created
    /// by this instance are placed relative to the `anchor_pb`.
    pub fn new(
        registry: Arc<OperationRegistry>,
        multi_progress: MultiProgress,
        anchor_pb: ProgressBar,
    ) -> Self {
        Self::new_with_sink(
            registry,
            multi_progress,
            anchor_pb,
            Arc::new(NoopSink) as SinkHandle,
        )
    }

    /// Same as [`Self::new`] but lets the caller supply the event sink
    /// directly (skipping the env-var lookup).
    pub fn new_with_sink(
        registry: Arc<OperationRegistry>,
        multi_progress: MultiProgress,
        anchor_pb: ProgressBar,
        event_sink: SinkHandle,
    ) -> Self {
        let repodata_reporter = RepodataReporter::new(
            multi_progress.clone(),
            pixi_progress::ProgressBarPlacement::Before(anchor_pb.clone()),
            "fetching repodata".to_owned(),
        );
        let conda_solve_reporter = MainProgressBar::new(
            multi_progress.clone(),
            pixi_progress::ProgressBarPlacement::Before(anchor_pb.clone()),
            "solving".to_owned(),
        );
        let install_reporter = SyncReporter::new(
            registry.clone(),
            multi_progress.clone(),
            pixi_progress::ProgressBarPlacement::Before(anchor_pb.clone()),
        );
        let source_checkout_reporter = Arc::new(GitCheckoutProgress::new(
            registry.clone(),
            multi_progress.clone(),
            anchor_pb.clone(),
        ));
        Self {
            registry,
            source_checkout_reporter,
            conda_solve_reporter,
            solve_bars: Mutex::new(HashMap::new()),
            repodata_reporter,
            sync_reporter: install_reporter,
            event_sink,
        }
    }

    /// Register every sub-reporter this instance owns into the
    /// dispatcher builder. The git-checkout slot gets its inner
    /// progress Arc directly; the rest go through `self`. When the
    /// event sink is enabled, each reporter is wrapped in its
    /// `Streaming*` decorator so events are emitted alongside the
    /// indicatif output.
    pub fn register_with(
        self: Arc<Self>,
        builder: CommandDispatcherBuilder,
    ) -> CommandDispatcherBuilder {
        let registry = self.registry.clone();
        let sink = self.event_sink.clone();
        let backend_source_build_inner: Arc<
            dyn pixi_command_dispatcher::BackendSourceBuildReporter,
        > = Arc::new(self.sync_reporter.clone());
        let backend_source_build_reporter: Arc<
            dyn pixi_command_dispatcher::BackendSourceBuildReporter,
        > = Arc::new(StreamingBackendSourceBuildReporter {
            inner: backend_source_build_inner,
            sink: sink.clone(),
            registry: registry.clone(),
        });

        let pixi_solve: Arc<dyn pixi_command_dispatcher::PixiSolveReporter> =
            Arc::new(StreamingPixiSolveReporter {
                inner: self.clone(),
                sink: sink.clone(),
                registry: registry.clone(),
            });
        let conda_solve: Arc<dyn pixi_command_dispatcher::CondaSolveReporter> =
            Arc::new(StreamingCondaSolveReporter {
                inner: self.clone(),
                sink: sink.clone(),
                registry: registry.clone(),
            });
        let pixi_install: Arc<dyn pixi_command_dispatcher::PixiInstallReporter> =
            Arc::new(StreamingPixiInstallReporter {
                inner: self.clone(),
                sink: sink.clone(),
                registry: registry.clone(),
            });
        let instantiate_backend: Arc<dyn pixi_command_dispatcher::InstantiateBackendReporter> =
            Arc::new(StreamingInstantiateBackendReporter {
                inner: self.clone(),
                sink: sink.clone(),
                registry: registry.clone(),
            });
        let gateway: Arc<dyn pixi_command_dispatcher::GatewayReporter> =
            Arc::new(StreamingGatewayReporter {
                inner: self.clone(),
                sink: sink.clone(),
                registry: registry.clone(),
            });

        builder
            .with_pixi_solve_reporter(pixi_solve)
            .with_conda_solve_reporter(conda_solve)
            .with_pixi_install_reporter(pixi_install)
            .with_instantiate_backend_reporter(instantiate_backend)
            .with_git_checkout_reporter(self.source_checkout_reporter.clone())
            .with_backend_source_build_reporter(backend_source_build_reporter)
            .with_gateway_reporter(gateway)
    }

    /// Clear the current progress bars without tearing down the reporter.
    pub fn on_clear(&self) {
        self.conda_solve_reporter.clear();
        self.repodata_reporter.clear();
        self.sync_reporter.clear();
    }

    fn alloc(&self) -> OperationId {
        self.registry.allocate()
    }

    fn solve_bar(&self, id: OperationId) -> Option<usize> {
        self.solve_bars.lock().get(&id).copied()
    }
}


impl pixi_command_dispatcher::PixiInstallReporter for TopLevelProgress {
    fn on_queued(&self, _env: &InstallPixiEnvironmentSpec) -> OperationId {
        self.alloc()
    }

    fn on_started(&self, _install_id: OperationId) {}

    fn on_finished(&self, _install_id: OperationId) {}

    fn create_install_reporter(&self) -> Option<Box<dyn rattler::install::Reporter>> {
        Some(Box::new(self.sync_reporter.create_reporter()))
    }
}

impl pixi_command_dispatcher::InstantiateBackendReporter for TopLevelProgress {
    fn on_queued(&self, _spec: &pixi_build_discovery::JsonRpcBackendSpec) -> OperationId {
        self.alloc()
    }

    fn on_started(&self, _id: OperationId) {}

    fn on_finished(&self, _id: OperationId) {}

    fn create_install_reporter(&self) -> Option<Box<dyn rattler::install::Reporter>> {
        Some(Box::new(self.sync_reporter.create_reporter()))
    }
}

impl pixi_command_dispatcher::PixiSolveReporter for TopLevelProgress {
    fn on_queued(&self, env: &PixiSolveEnvironmentSpec) -> OperationId {
        if env.has_direct_conda_dependency {
            // Dependencies on conda packages trigger package-cache
            // validation via rayon; ensure it's initialized through the
            // uv path before the validation work starts.
            LazyLock::force(&RAYON_INITIALIZE);
        }

        let id = self.alloc();
        let bar = self
            .conda_solve_reporter
            .queued(format!("{} ({})", env.name, env.platform));
        self.solve_bars.lock().insert(id, bar);
        id
    }

    fn on_started(&self, _solve_id: OperationId) {}

    fn on_finished(&self, solve_id: OperationId) {
        self.solve_bars.lock().remove(&solve_id);
    }
}

impl pixi_command_dispatcher::GatewayReporter for TopLevelProgress {
    fn on_queued(
        &self,
        _spec: &GatewayQuerySpec,
    ) -> (
        OperationId,
        Option<Box<dyn rattler_repodata_gateway::Reporter>>,
    ) {
        let id = self.alloc();
        // Hand back the long-lived RepodataReporter so the indicatif
        // "fetching repodata" bar finally tracks downloads. It's
        // shared across queries by design (one bar, all downloads).
        let rep: Box<dyn rattler_repodata_gateway::Reporter> =
            Box::new(self.repodata_reporter.clone());
        (id, Some(rep))
    }

    fn on_started(&self, _id: OperationId) {}

    fn on_finished(&self, _id: OperationId) {}
}

impl pixi_command_dispatcher::CondaSolveReporter for TopLevelProgress {
    fn on_queued(&self, env: &SolveCondaEnvironmentSpec) -> OperationId {
        // Reuse the parent pixi-solve's bar slot when one is active so
        // a conda solve nested inside a pixi solve renders as the same
        // entry rather than a fresh row.
        let id = self.alloc();
        let parent_bar = self
            .registry
            .ancestors(id)
            .find_map(|ancestor| self.solve_bar(ancestor));
        let bar = match parent_bar {
            Some(bar) => bar,
            None => self
                .conda_solve_reporter
                .queued(env.name.clone().unwrap_or_default()),
        };
        self.solve_bars.lock().insert(id, bar);
        id
    }

    fn on_started(&self, solve_id: OperationId) {
        if let Some(bar) = self.solve_bar(solve_id) {
            self.conda_solve_reporter.start(bar);
        }
    }

    fn on_finished(&self, solve_id: OperationId) {
        if let Some(bar) = self.solve_bars.lock().remove(&solve_id) {
            self.conda_solve_reporter.finish(bar);
        }
    }
}

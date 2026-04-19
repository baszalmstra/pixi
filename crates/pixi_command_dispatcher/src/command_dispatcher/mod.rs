//! Defines the [`CommandDispatcher`] and its associated components for managing
//! and synchronizing tasks across different environments.
//!
//! The [`CommandDispatcher`] is a central component for orchestrating tasks
//! such as solving environments, fetching metadata, and managing source
//! checkouts. It ensures efficient execution by avoiding redundant computations
//! and supporting concurrent operations.

use std::sync::Arc;

pub use builder::{CommandDispatcherBuilder, ReporterContextSpawnHook};
pub use error::{CommandDispatcherError, CommandDispatcherErrorResultExt, ComputeResultExt};
pub use instantiate_backend::{InstantiateBackendError, InstantiateBackendSpec};
use pixi_build_frontend::BackendOverride;
use pixi_compute_engine::ComputeEngine;
use pixi_git::resolver::GitResolver;
use pixi_glob::GlobHashCache;
use pixi_record::PixiRecord;
use pixi_url::UrlResolver;
use rattler::package_cache::PackageCache;
use rattler_conda_types::{GenericVirtualPackage, Platform};
use rattler_networking::LazyClient;
use rattler_repodata_gateway::Gateway;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    BuildBackendMetadata, BuildBackendMetadataError, BuildBackendMetadataSpec, DevSourceMetadata,
    DevSourceMetadataError, DevSourceMetadataSpec, Executor, PixiEnvironmentSpec,
    ResolvedSourceRecord, SolveCondaEnvironmentSpec, SolvePixiEnvironmentError,
    SourceBuildCacheEntry, SourceBuildCacheStatusError, SourceBuildCacheStatusSpec, SourceMetadata,
    SourceMetadataError, SourceMetadataSpec, SourceRecordError, SourceRecordSpec,
    backend_source_build::{BackendBuiltSource, BackendSourceBuildError, BackendSourceBuildSpec},
    build::BuildCache,
    cache::{build_backend_metadata::BuildBackendMetadataCache, source_record::SourceRecordCache},
    cache_dirs::CacheDirs,
    install_pixi::{
        InstallPixiEnvironmentError, InstallPixiEnvironmentResult, InstallPixiEnvironmentSpec,
    },
    instantiate_tool_env::{
        InstantiateToolEnvironmentError, InstantiateToolEnvironmentResult,
        InstantiateToolEnvironmentSpec,
    },
    limits::ResolvedLimits,
    solve_conda::SolveCondaEnvironmentError,
    source_build::{SourceBuildError, SourceBuildResult, SourceBuildSpec},
};

mod builder;
mod error;
mod instantiate_backend;

/// The command dispatcher is responsible for synchronizing requests between
/// different conda environments.
#[derive(Clone)]
pub struct CommandDispatcher {
    /// The channel through which messages are sent to the command dispatcher.
    ///
    /// This is an option so we can drop this field in the `Drop`
    /// implementation. It should only ever be `None` when the command
    /// dispatcher is dropped.
    pub(crate) channel: Option<CommandDispatcherChannel>,

    /// The context in which the command dispatcher is operating. If a command
    /// dispatcher is created for a background task, this context will indicate
    /// from which task it was created.
    pub(crate) context: Option<CommandDispatcherContext>,

    /// Holds the shared data required by the command dispatcher.
    pub(crate) data: Arc<CommandDispatcherData>,

    /// The generic compute engine. Coexists with the legacy processor during
    /// migration; new Key-based operations run through this engine while
    /// legacy operations continue to use the channel-based processor.
    pub(crate) engine: ComputeEngine,

    /// Holds a strong reference to the process thread handle, this allows us to
    /// wait for the background thread to finish once the last (user facing)
    /// command dispatcher is dropped.
    pub(crate) processor_handle: Option<Arc<std::thread::JoinHandle<()>>>,
}

impl Drop for CommandDispatcher {
    fn drop(&mut self) {
        // Release our strong reference to the main thread channel. If this is the last
        // strong reference to the channel, the thread will shut down.
        drop(self.channel.take());

        // If this instance holds the last strong reference to the background thread
        // join handle this will await its shutdown.
        if let Some(handle) = self.processor_handle.take().and_then(Arc::into_inner) {
            let _err = handle.join();
        }
    }
}

/// Contains shared data required by the [`CommandDispatcher`].
///
/// This struct holds various components such as the gateway for querying
/// repodata, cache directories, and network clients.
pub(crate) struct CommandDispatcherData {
    /// The gateway to use to query conda repodata.
    pub gateway: Gateway,

    /// Backend metadata cache used to store metadata for source packages.
    pub build_backend_metadata_cache: BuildBackendMetadataCache,

    /// Source metadata cache used to store metadata for source packages.
    pub source_record_cache: SourceRecordCache,

    /// Build cache used to store build artifacts for source packages.
    pub build_cache: BuildCache,

    /// The resolver of git repositories.
    pub git_resolver: GitResolver,

    /// The resolver of url archives.
    pub url_resolver: UrlResolver,

    /// The location to store caches.
    pub cache_dirs: CacheDirs,

    /// The reqwest client to use for network requests.
    pub download_client: LazyClient,

    /// Backend overrides for build environments.
    pub build_backend_overrides: BackendOverride,

    /// A cache for glob hashes.
    pub glob_hash_cache: GlobHashCache,

    /// The resolved limits for the command dispatcher.
    pub limits: ResolvedLimits,

    /// The package cache used to store packages.
    pub package_cache: PackageCache,

    /// The platform (and virtual packages) to use for tools that should run on
    /// the current system. Usually this is the current platform, but it can
    /// be a different platform.
    pub tool_platform: (Platform, Vec<GenericVirtualPackage>),

    /// True if execution of link scripts is enabled.
    pub execute_link_scripts: bool,

    /// The execution type of the dispatcher.
    pub executor: Executor,

    /// Semaphore that bounds concurrent git checkouts driven through
    /// the compute engine. `None` means unbounded.
    pub git_checkout_semaphore: Option<Arc<Semaphore>>,

    /// Semaphore that bounds concurrent URL archive fetches driven
    /// through the compute engine. `None` means unbounded.
    pub url_checkout_semaphore: Option<Arc<Semaphore>>,
}

/// A channel through which to send any messages to the command_dispatcher. Some
/// dispatchers are constructed by the command_dispatcher itself. To avoid a
/// cyclic dependency, these "sub"-dispatchers use a weak reference to the
/// sender.
#[derive(Clone)]
pub(crate) enum CommandDispatcherChannel {
    Strong(mpsc::UnboundedSender<ForegroundMessage>),
    Weak(mpsc::WeakUnboundedSender<ForegroundMessage>),
}

impl CommandDispatcherChannel {
    /// Returns an owned channel that can be used to send messages to the
    /// background task, or `None` if the background task has been dropped.
    pub fn sender(&self) -> Option<mpsc::UnboundedSender<ForegroundMessage>> {
        match self {
            CommandDispatcherChannel::Strong(sender) => Some(sender.clone()),
            CommandDispatcherChannel::Weak(sender) => sender.upgrade(),
        }
    }
}

/// Context in which the [`CommandDispatcher`] is operating.
///
/// This enum is used to track dependencies and associate tasks with specific
/// contexts.
#[derive(Debug, Copy, Clone, derive_more::From, derive_more::TryInto, Hash, Eq, PartialEq)]
pub(crate) enum CommandDispatcherContext {
    SolveCondaEnvironment(SolveCondaEnvironmentId),
    SolvePixiEnvironment(SolvePixiEnvironmentId),
    BuildBackendMetadata(BuildBackendMetadataId),
    BackendSourceBuild(BackendSourceBuildId),
    SourceMetadata(SourceMetadataId),
    SourceRecord(SourceRecordId),
    SourceBuild(SourceBuildId),
    QuerySourceBuildCache(SourceBuildCacheStatusId),
    DevSourceMetadata(DevSourceMetadataId),
    InstallPixiEnvironment(InstallPixiEnvironmentId),
    InstantiateToolEnv(InstantiatedToolEnvId),
}

slotmap::new_key_type! {
    /// An id that uniquely identifies a conda environment that is being solved.
    pub(crate) struct SolveCondaEnvironmentId;

    /// An id that uniquely identifies a build backend source build request.
    pub(crate) struct BackendSourceBuildId;

    /// An id that uniquely identifies a conda environment that is being solved.
    pub(crate) struct SolvePixiEnvironmentId;

    /// An id that uniquely identifies an installation of an environment.
    pub(crate) struct InstallPixiEnvironmentId;

}

/// An id that uniquely identifies a build backend metadata request.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct BuildBackendMetadataId(pub usize);

/// An id that uniquely identifies a source metadata request.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct SourceMetadataId(pub usize);

/// An id that uniquely identifies a source record request.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct SourceRecordId(pub usize);

/// An id that uniquely identifies a source build request.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct SourceBuildId(pub usize);

/// An id that uniquely identifies a source build cache request.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct SourceBuildCacheStatusId(pub usize);

/// An id that uniquely identifies a dev source metadata request.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct DevSourceMetadataId(pub usize);

/// An id that uniquely identifies a tool environment.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct InstantiatedToolEnvId(pub usize);

/// A message send to the dispatch task.
#[allow(clippy::large_enum_variant)]
#[derive(derive_more::From)]
pub(crate) enum ForegroundMessage {
    SolveCondaEnvironment(SolveCondaEnvironmentTask),
    SolvePixiEnvironment(SolvePixiEnvironmentTask),
    BuildBackendMetadata(BuildBackendMetadataTask),
    BackendSourceBuild(BackendSourceBuildTask),
    SourceMetadata(SourceMetadataTask),
    SourceRecord(SourceRecordTask),
    SourceBuild(SourceBuildTask),
    QuerySourceBuildCache(SourceBuildCacheStatusTask),
    DevSourceMetadata(DevSourceMetadataTask),
    InstallPixiEnvironment(InstallPixiEnvironmentTask),
    InstantiateToolEnvironment(Task<InstantiateToolEnvironmentSpec>),
    ClearReporter(oneshot::Sender<()>),
    #[from(ignore)]
    ClearFilesystemCaches(oneshot::Sender<()>),
}

/// A message that is send to the background task to start solving a particular
/// pixi environment.
pub(crate) type SolvePixiEnvironmentTask = Task<PixiEnvironmentSpec>;
impl TaskSpec for PixiEnvironmentSpec {
    type Output = Vec<PixiRecord>;
    type Error = SolvePixiEnvironmentError;
}

/// A message that is send to the background task to install a particular
/// pixi environment.
pub(crate) type InstallPixiEnvironmentTask = Task<InstallPixiEnvironmentSpec>;
impl TaskSpec for InstallPixiEnvironmentSpec {
    type Output = InstallPixiEnvironmentResult;
    type Error = InstallPixiEnvironmentError;
}

/// A message that is send to the background task to start solving a particular
/// conda environment.
pub(crate) type SolveCondaEnvironmentTask = Task<SolveCondaEnvironmentSpec>;
impl TaskSpec for SolveCondaEnvironmentSpec {
    type Output = Vec<PixiRecord>;
    type Error = SolveCondaEnvironmentError;
}

/// A message that is send to the background task to requesting the metadata for
/// a particular source spec.
pub(crate) type BuildBackendMetadataTask = Task<BuildBackendMetadataSpec>;

impl TaskSpec for BuildBackendMetadataSpec {
    type Output = Arc<BuildBackendMetadata>;
    type Error = BuildBackendMetadataError;
}

pub(crate) type SourceMetadataTask = Task<SourceMetadataSpec>;
impl TaskSpec for SourceMetadataSpec {
    type Output = Arc<SourceMetadata>;
    type Error = SourceMetadataError;
}

pub(crate) type SourceRecordTask = Task<SourceRecordSpec>;
impl TaskSpec for SourceRecordSpec {
    type Output = Arc<ResolvedSourceRecord>;
    type Error = SourceRecordError;
}

pub(crate) type SourceBuildTask = Task<SourceBuildSpec>;

impl TaskSpec for SourceBuildSpec {
    type Output = SourceBuildResult;
    type Error = SourceBuildError;
}

pub(crate) type BackendSourceBuildTask = Task<Box<BackendSourceBuildSpec>>;

impl TaskSpec for Box<BackendSourceBuildSpec> {
    type Output = BackendBuiltSource;
    type Error = BackendSourceBuildError;
}

/// Instantiates a tool environment.
impl TaskSpec for InstantiateToolEnvironmentSpec {
    type Output = InstantiateToolEnvironmentResult;
    type Error = InstantiateToolEnvironmentError;
}

pub(crate) type SourceBuildCacheStatusTask = Task<SourceBuildCacheStatusSpec>;

impl TaskSpec for SourceBuildCacheStatusSpec {
    type Output = Arc<SourceBuildCacheEntry>;
    type Error = SourceBuildCacheStatusError;
}

pub(crate) type DevSourceMetadataTask = Task<DevSourceMetadataSpec>;

impl TaskSpec for DevSourceMetadataSpec {
    type Output = DevSourceMetadata;
    type Error = DevSourceMetadataError;
}

impl Default for CommandDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandDispatcher {
    /// Constructs a new default constructed instance.
    pub fn new() -> Self {
        Self::builder().finish()
    }

    /// Constructs a new builder for the command dispatcher.
    pub fn builder() -> CommandDispatcherBuilder {
        CommandDispatcherBuilder::default()
    }

    /// Returns the executor used by the command dispatcher.
    pub fn executor(&self) -> Executor {
        self.data.executor
    }

    /// Returns a reference to the compute engine.
    ///
    /// The engine handles Key-based computations with automatic
    /// deduplication, caching, and cycle detection. During migration,
    /// it coexists with the legacy channel-based processor.
    pub fn engine(&self) -> &ComputeEngine {
        &self.engine
    }

    /// Returns the cache for source metadata.
    pub fn build_backend_metadata_cache(&self) -> &BuildBackendMetadataCache {
        &self.data.build_backend_metadata_cache
    }

    /// Returns the cache for source metadata.
    pub fn source_record_cache(&self) -> &SourceRecordCache {
        &self.data.source_record_cache
    }

    /// Returns the build cache for source packages.
    pub fn build_cache(&self) -> &BuildCache {
        &self.data.build_cache
    }

    /// Returns the gateway used to query conda repodata.
    pub fn gateway(&self) -> &Gateway {
        &self.data.gateway
    }

    /// Returns any build backend overrides.
    pub fn build_backend_overrides(&self) -> &BackendOverride {
        &self.data.build_backend_overrides
    }

    /// Returns the cache directories used by the command dispatcher.
    pub fn cache_dirs(&self) -> &CacheDirs {
        &self.data.cache_dirs
    }

    /// Returns the glob hash cache.
    pub fn glob_hash_cache(&self) -> &GlobHashCache {
        &self.data.glob_hash_cache
    }

    /// Returns the channel configuration injected into the compute
    /// engine at construction. Panics if no value was injected, matching
    /// the [`pixi_compute_engine::InjectedKey`] contract.
    pub fn channel_config(&self) -> Arc<rattler_conda_types::ChannelConfig> {
        self.engine
            .read(&crate::ChannelConfigKey)
            .expect("ChannelConfig must be injected on the compute engine")
    }

    /// Returns the build-protocol discovery configuration injected into
    /// the compute engine at construction.
    pub fn enabled_protocols(&self) -> Arc<pixi_build_discovery::EnabledProtocols> {
        self.engine
            .read(&crate::EnabledProtocolsKey)
            .expect("EnabledProtocols must be injected on the compute engine")
    }

    /// Discovers the build backend for a source path via the compute
    /// engine. Deduplicated and cached by `DiscoveredBackendKey`; the
    /// channel configuration and enabled protocols come from the
    /// engine-wide injected values.
    pub async fn discovered_backend(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<
        Arc<pixi_build_discovery::DiscoveredBackend>,
        CommandDispatcherError<Arc<pixi_build_discovery::DiscoveryError>>,
    > {
        let key = crate::DiscoveredBackendKey::new(path);
        self.engine
            .with_ctx(async |ctx| ctx.compute(&key).await)
            .await
            .map_err_into_dispatcher(std::convert::identity)
    }

    /// Clears in-memory caches whose correctness depends on the filesystem.
    ///
    /// This invalidates memoized results that are derived from files on disk so
    /// subsequent operations re-check the current state of the filesystem. It:
    /// - clears glob hash memoization (`GlobHashCache`) used for input file hashing
    /// - clears memoized SourceBuildCacheStatus results held by the processor,
    ///   while preserving any in-flight queries
    pub async fn clear_filesystem_caches(&self) {
        if let Some(sender) = self.channel().sender() {
            let (tx, rx) = oneshot::channel();
            let _ = sender.send(ForegroundMessage::ClearFilesystemCaches(tx));
            let _ = rx.await;
        }
    }

    /// Returns the download client used by the command dispatcher.
    pub fn download_client(&self) -> &LazyClient {
        &self.data.download_client
    }

    /// Returns the package cache used by the command dispatcher.
    pub fn package_cache(&self) -> &PackageCache {
        &self.data.package_cache
    }

    /// Returns the platform and virtual packages used for tool environments.
    pub fn tool_platform(&self) -> (Platform, &[GenericVirtualPackage]) {
        (self.data.tool_platform.0, &self.data.tool_platform.1)
    }

    /// Returns true if execution of link scripts is enabled.
    pub fn allow_execute_link_scripts(&self) -> bool {
        self.data.execute_link_scripts
    }

    /// Returns the channel used to send messages to the command dispatcher.
    fn channel(&self) -> &CommandDispatcherChannel {
        self.channel
            .as_ref()
            .expect("command dispatcher has been dropped")
    }

    /// Sends a task to the command dispatcher and waits for the result.
    async fn execute_task<T: TaskSpec>(
        &self,
        spec: T,
    ) -> Result<T::Output, CommandDispatcherError<T::Error>>
    where
        ForegroundMessage: From<Task<T>>,
    {
        let Some(sender) = self.channel().sender() else {
            // If this fails, it means the command dispatcher was dropped and the task is
            // immediately canceled.
            return Err(CommandDispatcherError::Cancelled);
        };

        let cancellation_token = CancellationToken::new();
        let (tx, rx) = oneshot::channel();
        sender
            .send(ForegroundMessage::from(Task {
                spec,
                parent: self.context,
                tx,
                cancellation_token: cancellation_token.clone(),
            }))
            .map_err(|_| CommandDispatcherError::Cancelled)?;

        // Make sure to trigger the cancellation token when this async task is dropped.
        let _cancel_guard = cancellation_token.drop_guard();

        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(err)) => Err(CommandDispatcherError::Failed(err)),
            Err(_) => Err(CommandDispatcherError::Cancelled),
        }
    }

    /// Notifies the progress reporter that it should clear its output.
    pub async fn clear_reporter(&self) {
        let Some(sender) = self.channel().sender() else {
            // If this fails, it means the command dispatcher was dropped and the task is
            // immediately canceled.
            return;
        };
        let (tx, rx) = oneshot::channel();
        let _ = sender.send(ForegroundMessage::ClearReporter(tx));
        let _ = rx.await;
    }

    /// Returns the metadata of the source spec.
    pub async fn build_backend_metadata(
        &self,
        spec: BuildBackendMetadataSpec,
    ) -> Result<Arc<BuildBackendMetadata>, CommandDispatcherError<BuildBackendMetadataError>> {
        self.execute_task(spec).await
    }

    /// Returns the metadata of a particular source package (all variants).
    ///
    /// Fans out to [`Self::source_record`] for each variant output returned
    /// by the build backend.
    pub async fn source_metadata(
        &self,
        spec: SourceMetadataSpec,
    ) -> Result<Arc<SourceMetadata>, CommandDispatcherError<SourceMetadataError>> {
        self.execute_task(spec).await
    }

    /// Returns the resolved source record for a specific package name + variant
    /// combination.
    pub async fn source_record(
        &self,
        spec: SourceRecordSpec,
    ) -> Result<Arc<ResolvedSourceRecord>, CommandDispatcherError<SourceRecordError>> {
        self.execute_task(spec).await
    }

    /// Returns the metadata for dev sources.
    ///
    /// This method queries the build backend for all outputs from a dev source
    /// and creates DevSourceRecords for each one. These records contain the
    /// combined dependencies (build, host, run) for each output.
    ///
    /// Unlike `source_metadata`, this is specifically for dev sources
    /// where the dependencies are installed but the package itself is not built.
    ///
    /// # Requirements
    ///
    /// - The build backend must support the `conda/outputs` procedure (API v1+)
    pub async fn dev_source_metadata(
        &self,
        spec: DevSourceMetadataSpec,
    ) -> Result<DevSourceMetadata, CommandDispatcherError<DevSourceMetadataError>> {
        self.execute_task(spec).await
    }

    /// Query the source build cache for a particular source package.
    pub async fn source_build_cache_status(
        &self,
        spec: SourceBuildCacheStatusSpec,
    ) -> Result<Arc<SourceBuildCacheEntry>, CommandDispatcherError<SourceBuildCacheStatusError>>
    {
        self.execute_task(spec).await
    }

    /// Builds the source package and returns the built conda package.
    pub async fn source_build(
        &self,
        spec: SourceBuildSpec,
    ) -> Result<SourceBuildResult, CommandDispatcherError<SourceBuildError>> {
        self.execute_task(spec).await
    }

    ///
    /// Calls into a pixi build backend to perform a source build.
    pub(crate) async fn backend_source_build(
        &self,
        spec: BackendSourceBuildSpec,
    ) -> Result<BackendBuiltSource, CommandDispatcherError<BackendSourceBuildError>> {
        self.execute_task(Box::new(spec)).await
    }

    /// Solves a particular pixi environment specified by `PixiEnvironmentSpec`.
    ///
    /// This function processes all package requirements defined in the spec,
    /// handling both binary and source packages. For source packages, it:
    ///
    /// 1. Checks out source code repositories
    /// 2. Builds necessary environments for processing source dependencies
    /// 3. Queries metadata from source packages
    /// 4. Recursively processes any transitive source dependencies
    ///
    /// The function automatically deduplicates work when the same source is
    /// referenced multiple times, and ensures efficient parallel execution
    /// where possible.
    pub async fn solve_pixi_environment(
        &self,
        spec: PixiEnvironmentSpec,
    ) -> Result<Vec<PixiRecord>, CommandDispatcherError<SolvePixiEnvironmentError>> {
        self.execute_task(spec).await
    }

    /// Install a pixi environment.
    ///
    /// This method takes a previously solved environment specification and
    /// installs all required packages into the target prefix. It handles
    /// both binary packages (from conda repositories) and source packages
    /// (built from source code).
    pub async fn install_pixi_environment(
        &self,
        spec: InstallPixiEnvironmentSpec,
    ) -> Result<InstallPixiEnvironmentResult, CommandDispatcherError<InstallPixiEnvironmentError>>
    {
        self.execute_task(spec).await
    }

    /// Solves a particular conda environment.
    ///
    /// This method processes a complete environment specification containing
    /// both binary and source packages to find a compatible set of packages
    /// that satisfy all requirements and constraints.
    ///
    /// Unlike solving pixi environments, this method does not perform recursive
    /// source resolution and querying repodata as all information is already
    /// available in the specification.
    pub async fn solve_conda_environment(
        &self,
        spec: SolveCondaEnvironmentSpec,
    ) -> Result<Vec<PixiRecord>, CommandDispatcherError<SolveCondaEnvironmentError>> {
        self.execute_task(spec).await
    }

    /// Instantiates an environment for a tool based on the given spec. Reuses
    /// the environment if possible.
    ///
    /// This method creates isolated environments for build backends and other
    /// tools. These environments are specialized containers with specific
    /// packages needed for particular tasks like building packages,
    /// extracting metadata, or running tools.
    pub async fn instantiate_tool_environment(
        &self,
        spec: InstantiateToolEnvironmentSpec,
    ) -> Result<
        InstantiateToolEnvironmentResult,
        CommandDispatcherError<InstantiateToolEnvironmentError>,
    > {
        self.execute_task(spec).await
    }
}

/// Defines the inputs and outputs of a certain foreground task specification.
pub(crate) trait TaskSpec {
    type Output;
    type Error;
}

pub(crate) struct Task<S: TaskSpec> {
    pub spec: S,
    pub parent: Option<CommandDispatcherContext>,
    pub tx: oneshot::Sender<Result<S::Output, S::Error>>,
    pub cancellation_token: CancellationToken,
}

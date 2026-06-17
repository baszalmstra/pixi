//! The top-level [`ComputeEngine`] and its internal state.

use std::{collections::HashSet, sync::Arc};

use futures::future::BoxFuture;

use crate::{
    AnyKey, ComputeCtx, ComputeEngineBuilder, ComputeError, DataStore, GraphVersion, InjectedKey,
    Key, StorageType, UpdateError,
    cycle::active_edges::ActiveEdges,
    graph_owner::{GraphChange, GraphOwner},
    versions::VersionEpoch,
};

/// A hook invoked at every compute-task spawn site, just before the
/// engine calls [`tokio::spawn`].
///
/// [`SpawnHook::wrap`] is called **synchronously in the spawning
/// task's context**, so it can snapshot caller-side task-locals (e.g.
/// a tracing span or a reporter context). The future it returns is
/// what gets spawned; a typical implementation wraps `fut` with
/// `tokio::task_local!`'s `LocalKey::scope` to install the captured
/// task-local into the spawned task for its entire lifetime.
///
/// The future is required to be `'static` because it is handed to
/// [`tokio::spawn`]; the engine boxes it at the spawn site so
/// implementations do not need to impose a tighter bound.
pub trait SpawnHook: Send + Sync + 'static {
    /// Wrap the compute body future before it is spawned. The
    /// returned future's output type must remain `()`; the engine
    /// carries the typed compute result out of band.
    fn wrap(&self, data: &DataStore, fut: BoxFuture<'static, ()>) -> BoxFuture<'static, ()>;
}

/// The top-level compute engine.
///
/// An engine is a handle (internally `Arc`-based) and can be freely cloned.
/// All clones share the same dedup / completed-value cache, so two handles
/// handed to different tasks will observe each other's results.
///
/// # Lifecycle
///
/// 1. Create an engine with [`ComputeEngine::new`] (or [`Default`]).
/// 2. From any async context backed by a tokio runtime, call
///    [`ComputeEngine::compute`] with a reference to a [`Key`]. The engine
///    returns the cached value if one exists, joins an in-flight compute
///    if one is running, or spawns a fresh compute otherwise.
/// 3. Clone the engine freely to share across tasks; all clones see the
///    same cache.
///
/// # Runtime requirements
///
/// The engine uses [`tokio::spawn`] to drive each compute. All calls to
/// [`ComputeEngine::compute`] must happen from within a tokio runtime.
///
/// # Example
///
/// ```
/// use std::fmt;
/// use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
///
/// #[derive(Clone, Debug, Hash, PartialEq, Eq)]
/// struct Double(u32);
///
/// impl fmt::Display for Double {
///     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
///         write!(f, "{}", self.0)
///     }
/// }
///
/// impl Key for Double {
///     type Value = u32;
///     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value {
///         self.0 * 2
///     }
/// }
///
/// # tokio_test::block_on(async {
/// let engine = ComputeEngine::new();
/// assert_eq!(engine.compute(&Double(21)).await.unwrap(), 42);
///
/// // A clone shares the cache: the second compute does not re-run.
/// let shared = engine.clone();
/// assert_eq!(shared.compute(&Double(21)).await.unwrap(), 42);
/// # });
/// ```
#[derive(Clone)]
pub struct ComputeEngine {
    pub(crate) inner: Arc<EngineInner>,
}

/// Read-only compute handle with an active graph-version lease.
#[derive(Clone)]
pub struct ComputeSnapshot {
    lease: Arc<SnapshotLease>,
}

pub struct ComputeUpdater {
    engine: ComputeEngine,
    seen: HashSet<AnyKey>,
    changes: Vec<GraphChange>,
    error: Option<UpdateError>,
}

/// Summary of one committed update transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateResult {
    pub previous_version: GraphVersion,
    pub current_version: GraphVersion,
    pub changed: bool,
    pub change_count: usize,
}

/// Point-in-time daemon-facing counters for the compute engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeEngineStats {
    pub current_version: GraphVersion,
    pub invalid_before: GraphVersion,
    pub active_versions: usize,
    pub active_leases: usize,
    pub interned_keys: usize,
    pub node_count: usize,
    pub completed_nodes: usize,
    pub completed_versions: usize,
    pub injected_nodes: usize,
    pub injected_versions: usize,
    pub in_flight_count: usize,
    pub stale_in_flight_count: usize,
    pub dirty_keys: usize,
    pub dirty_entries: usize,
    pub rdep_keys: usize,
    pub rdep_edges: usize,
    pub unchanged_keys: usize,
    pub unchanged_ranges: usize,
}

struct SnapshotLease {
    engine: Arc<EngineInner>,
    version: GraphVersion,
    epoch: VersionEpoch,
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        self.engine.graph.release_version(self.version, self.epoch);
    }
}

/// Engine state shared between every [`ComputeEngine`] clone and every
/// [`ComputeCtx`] spawned by a compute.
pub(crate) struct EngineInner {
    /// Serialized keyed graph storage. Doubles as the value cache and
    /// the dependency graph.
    pub(crate) graph: GraphOwner,
    /// Global active-edge graph used by synchronous cycle detection
    /// in [`ComputeCtx::compute`](crate::ComputeCtx::compute). Each
    /// edge carries the notify target that was active when the edge
    /// was created, so detection does not need a separate per-key
    /// guard registry to route cycles to cross-task scopes.
    pub(crate) active_edges: Arc<ActiveEdges>,
    /// Set via [`ComputeEngineBuilder::sequential_branches`]. When
    /// `true`, the parallel combinators on [`ComputeCtx`] run their
    /// branches one at a time in mint order instead of concurrently.
    pub(crate) sequential_branches: bool,
    /// Engine-wide shared data, set at construction time.
    pub(crate) global_data: DataStore,
    /// Optional hook invoked on every compute-task spawn. See
    /// [`SpawnHook`] for the calling protocol.
    pub(crate) spawn_hook: Option<Arc<dyn SpawnHook>>,
}

impl Default for ComputeEngine {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ComputeSnapshot {
    /// The graph version associated with this snapshot.
    pub fn version(&self) -> GraphVersion {
        self.lease.version
    }

    /// Compute the value for `key` using this snapshot's context.
    pub fn compute<K: Key>(
        &self,
        key: &K,
    ) -> impl Future<Output = Result<K::Value, ComputeError>> + use<K> {
        let lease = self.lease.clone();
        let key = key.clone();
        async move {
            let _lease = lease.clone();
            if lease.engine.graph.should_reject(lease.version) {
                return Err(ComputeError::Rejected);
            }
            let mut ctx = ComputeCtx::new(lease.engine.clone(), lease.version, lease.epoch);
            ctx.compute_root(&key).await
        }
    }

    /// Run `f` with a fresh [`ComputeCtx`] bound to this snapshot.
    pub async fn with_ctx<F, T>(&self, f: F) -> Result<T, ComputeError>
    where
        F: AsyncFnOnce(&mut ComputeCtx) -> T,
    {
        let lease = self.lease.clone();
        if lease.engine.graph.should_reject(lease.version) {
            return Err(ComputeError::Rejected);
        }
        let (mut ctx, fallback_rx) =
            ComputeCtx::new_root_with_fallback(lease.engine.clone(), lease.version, lease.epoch);
        let _lease = lease;

        tokio::select! {
            biased;
            cycle = fallback_rx => Err(ComputeError::Cycle(
                cycle.expect("root fallback sender dropped while scope active"),
            )),
            value = f(&mut ctx) => Ok(value),
        }
    }
}

impl ComputeUpdater {
    /// Record a key as changed in this update transaction.
    pub fn changed<K: Key>(&mut self, key: K) -> Result<(), UpdateError> {
        let any_key = AnyKey::new(key.clone());
        if K::storage_type() == StorageType::Injected {
            return self.fail(UpdateError::InjectedKeyInvalidated {
                key: any_key.to_string(),
            });
        }
        self.push(any_key, GraphChange::changed(key))
    }

    /// Record a key as changed unless this transaction already contains it.
    ///
    /// Returns `Ok(true)` when a new graph update was added and `Ok(false)`
    /// when the key was already present. This is useful for coalescing noisy
    /// external change notifications while keeping [`changed`](Self::changed)
    /// strict for callers that expect duplicates to be an error.
    pub fn changed_if_new<K: Key>(&mut self, key: K) -> Result<bool, UpdateError> {
        let any_key = AnyKey::new(key.clone());
        if K::storage_type() == StorageType::Injected {
            return self.fail(UpdateError::InjectedKeyInvalidated {
                key: any_key.to_string(),
            });
        }
        if self.seen.contains(&any_key) {
            return Ok(false);
        }
        self.push(any_key, GraphChange::changed(key))?;
        Ok(true)
    }

    /// Record a key/value replacement in this update transaction.
    pub fn changed_to<K: Key>(&mut self, key: K, value: K::Value) -> Result<(), UpdateError> {
        self.push(
            AnyKey::new(key.clone()),
            GraphChange::changed_to(key, value),
        )
    }

    /// Record an injected-key replacement in this update transaction.
    pub fn re_inject<K: InjectedKey>(
        &mut self,
        key: K,
        value: K::Value,
    ) -> Result<(), UpdateError> {
        self.changed_to(key, value)
    }

    /// Apply the recorded changes as one graph-version update.
    pub fn commit(self) -> Result<GraphVersion, UpdateError> {
        self.commit_report().map(|report| report.current_version)
    }

    /// Apply the recorded changes and return whether the batch changed graph state.
    pub fn commit_report(self) -> Result<UpdateResult, UpdateError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let change_count = self.changes.len();
        let result = self
            .engine
            .inner
            .graph
            .update_state_with_result(self.changes);
        Ok(UpdateResult {
            previous_version: result.previous_version,
            current_version: result.current_version,
            changed: result.changed,
            change_count,
        })
    }

    fn push(&mut self, key: AnyKey, change: GraphChange) -> Result<(), UpdateError> {
        if !self.seen.insert(key.clone()) {
            return self.fail(UpdateError::DuplicateKey {
                key: key.to_string(),
            });
        }
        self.changes.push(change);
        Ok(())
    }

    fn fail<T>(&mut self, error: UpdateError) -> Result<T, UpdateError> {
        if self.error.is_none() {
            self.error = Some(error.clone());
        }
        Err(error)
    }
}

impl ComputeEngine {
    /// Create a fresh engine with an empty cache and default settings.
    ///
    /// Equivalent to [`Default::default`]. The engine holds no tokio
    /// resources until the first [`compute`](Self::compute) call.
    /// For non-default settings (e.g. serialized sub-compute ordering
    /// for tests), use [`ComputeEngine::builder`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Start building a [`ComputeEngine`] with non-default settings.
    pub fn builder() -> ComputeEngineBuilder {
        ComputeEngineBuilder::new()
    }

    /// Create an update transaction for graph mutations.
    pub fn updater(&self) -> ComputeUpdater {
        ComputeUpdater {
            engine: self.clone(),
            seen: HashSet::new(),
            changes: Vec::new(),
            error: None,
        }
    }

    /// Create a read-only snapshot at the engine's current graph version.
    pub fn snapshot(&self) -> ComputeSnapshot {
        let (version, epoch) = self.inner.graph.acquire_current();
        ComputeSnapshot {
            lease: Arc::new(SnapshotLease {
                engine: self.inner.clone(),
                version,
                epoch,
            }),
        }
    }

    /// Return the engine's current graph version.
    pub fn current_version(&self) -> GraphVersion {
        self.inner.graph.current_version()
    }

    /// Return point-in-time daemon-facing engine counters.
    pub fn stats(&self) -> ComputeEngineStats {
        let stats = self.inner.graph.stats();
        ComputeEngineStats {
            current_version: stats.versions.current,
            invalid_before: stats.versions.invalid_before,
            active_versions: stats.versions.active_versions,
            active_leases: stats.versions.active_leases,
            interned_keys: stats.graph.interned_keys,
            node_count: stats.graph.node_count,
            completed_nodes: stats.graph.completed_nodes,
            completed_versions: stats.graph.completed_versions,
            injected_nodes: stats.graph.injected_nodes,
            injected_versions: stats.graph.injected_versions,
            in_flight_count: stats.graph.in_flight_count,
            stale_in_flight_count: stats.graph.stale_in_flight_count,
            dirty_keys: stats.graph.dirty_keys,
            dirty_entries: stats.graph.dirty_entries,
            rdep_keys: stats.graph.rdep_keys,
            rdep_edges: stats.graph.rdep_edges,
            unchanged_keys: stats.graph.unchanged_keys,
            unchanged_ranges: stats.graph.unchanged_ranges,
        }
    }

    /// Access immutable engine-wide data registered by the builder.
    pub fn global_data(&self) -> &crate::DataStore {
        &self.inner.global_data
    }

    /// Compute the value for `key`, deduping against any in-flight or
    /// previously-completed compute for the same key.
    ///
    /// # Caching behavior
    ///
    /// - **Completed cache hit**: the stored value is cloned and returned.
    /// - **In-flight**: the existing shared future is joined; compute
    ///   runs exactly once, all subscribers receive the same result.
    /// - **Miss**: a fresh tokio task is spawned to run the compute; its
    ///   shared future is installed in the in-flight cache, and on
    ///   completion the value is promoted to the completed cache.
    ///
    /// # Errors
    ///
    /// - [`ComputeError::Canceled`] if the underlying spawned task was
    ///   aborted before producing a value. This happens when the final
    ///   subscriber to an in-flight compute drops its handle.
    /// - [`ComputeError::Cycle`] if a dependency cycle was detected
    ///   that no
    ///   [`ComputeCtx::with_cycle_guard`](crate::ComputeCtx::with_cycle_guard)
    ///   scope on the cycle path caught. The wrapped
    ///   [`CycleError`](crate::CycleError) carries the full ring of
    ///   keys.
    ///
    /// The returned future uses precise capture (`use<K>`), so temporary
    /// key references like `engine.compute(&MyKey(..))` work seamlessly.
    ///
    /// # Do not call from within a compute body
    ///
    /// The root ctx this method builds has no `current` key, so any
    /// edges it would add are not seen by cycle detection. A nested
    /// call from inside a running [`Key::compute`]
    /// body can therefore create a cross-task dedup deadlock that the
    /// detector will not catch.
    ///
    /// Inside a `Key::compute` body, use
    /// [`ComputeCtx::compute`](crate::ComputeCtx::compute), which
    /// does participate in cycle detection.
    pub fn compute<K: Key>(
        &self,
        key: &K,
    ) -> impl Future<Output = Result<K::Value, ComputeError>> + use<K> {
        self.snapshot().compute(key)
    }

    /// Run `f` with a fresh [`ComputeCtx`] bound to this engine and
    /// return its value, or the first [`ComputeError`] that surfaces.
    ///
    /// This is the bridge from the [`ComputeEngine`] handle to APIs
    /// defined as extension traits on [`ComputeCtx`]: the ctx is
    /// available inside the closure, so `ctx.some_ext_method()` works
    /// without the caller having to be inside a [`Key::compute`] body.
    ///
    /// # Errors
    ///
    /// - [`ComputeError::Cycle`] if a cycle is detected below any
    ///   [`ctx.compute(..)`](ComputeCtx::compute) call made from
    ///   inside `f`. A synthetic fallback on this scope catches the
    ///   cycle, drops `f`'s future, and returns the ring of keys.
    /// - The root ctx has no caller key, so [`ComputeError::Canceled`]
    ///   cannot be produced here directly; it can still surface from
    ///   inside `f` through a [`Key::compute`] body if one of the
    ///   sub-computes it awaits is canceled.
    ///
    /// # Do not call from within a compute body
    ///
    /// Same caveat as [`ComputeEngine::compute`]: the ctx built here
    /// has no `current` key, so edges it adds are invisible to the
    /// cycle detector and a nested call from inside a running
    /// [`Key::compute`] body can produce a dedup deadlock that will
    /// not be caught. Inside a compute body, use the
    /// [`ComputeCtx`] you were handed.
    ///
    /// # Example
    ///
    /// ```
    /// use std::fmt;
    /// use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    ///
    /// #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// struct Double(u32);
    ///
    /// impl fmt::Display for Double {
    ///     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    ///         write!(f, "{}", self.0)
    ///     }
    /// }
    ///
    /// impl Key for Double {
    ///     type Value = u32;
    ///     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value {
    ///         self.0 * 2
    ///     }
    /// }
    ///
    /// # tokio_test::block_on(async {
    /// let engine = ComputeEngine::new();
    /// let sum = engine
    ///     .with_ctx(async |ctx| {
    ///         let a = ctx.compute(&Double(10)).await;
    ///         let b = ctx.compute(&Double(11)).await;
    ///         a + b
    ///     })
    ///     .await
    ///     .unwrap();
    /// assert_eq!(sum, 42);
    /// # });
    /// ```
    pub async fn with_ctx<F, T>(&self, f: F) -> Result<T, ComputeError>
    where
        F: AsyncFnOnce(&mut ComputeCtx) -> T,
    {
        let snapshot = self.snapshot();
        snapshot.with_ctx(f).await
    }

    /// Inject a value for an [`InjectedKey`].
    ///
    /// The value is stored directly in the engine's graph. Subsequent
    /// [`compute`](Self::compute) calls (or
    /// [`ComputeCtx::compute`](crate::ComputeCtx::compute) inside a
    /// Key's compute body) for this key will return the injected value
    /// immediately without spawning a task.
    ///
    /// # Panics
    ///
    /// Panics if `key` has already been injected on this engine.
    /// Use [`ComputeEngine::re_inject`] or an update transaction to
    /// replace an injected value and dirty dependents.
    ///
    /// # Example
    ///
    /// ```
    /// use std::fmt;
    /// use pixi_compute_engine::{ComputeEngine, InjectedKey};
    ///
    /// #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// struct Seed(u32);
    ///
    /// impl fmt::Display for Seed {
    ///     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    ///         write!(f, "{}", self.0)
    ///     }
    /// }
    ///
    /// impl InjectedKey for Seed {
    ///     type Value = u64;
    /// }
    ///
    /// let engine = ComputeEngine::new();
    /// engine.inject(Seed(1), 42);
    /// ```
    pub fn inject<K: InjectedKey>(&self, key: K, value: K::Value) {
        if let Err(error) = self.try_inject(key, value) {
            panic!("{error}");
        }
    }

    /// Try to inject an initial value for an [`InjectedKey`].
    pub fn try_inject<K: InjectedKey>(&self, key: K, value: K::Value) -> Result<(), UpdateError> {
        self.inner.graph.insert_injected::<K>(key, value)
    }

    /// Read an injected key synchronously, without recording a
    /// dependency.
    ///
    /// Returns `None` if the key has not been injected yet. Unlike
    /// [`ComputeCtx::compute`](crate::ComputeCtx::compute) (which
    /// panics on a missing injected key), this method is safe to call
    /// for optional lookups or pre-flight checks.
    ///
    /// # Caution
    ///
    /// This method does **not** record a dependency. Using it inside
    /// a Key's compute body would make the dependency invisible to
    /// introspection and prevent the engine from knowing that the
    /// parent needs to recompute when the injected value changes. Use
    /// [`ComputeCtx::compute`](crate::ComputeCtx::compute) there
    /// instead.
    pub fn read<K: InjectedKey>(&self, key: &K) -> Option<K::Value> {
        self.inner
            .graph
            .lookup_value::<K>(key.clone(), self.current_version())
    }

    /// Mark a computed key dirty and commit a graph version when graph state changes.
    pub fn invalidate<K: Key>(&self, key: &K) -> Result<GraphVersion, UpdateError> {
        let mut updater = self.updater();
        updater.changed(key.clone())?;
        updater.commit()
    }

    /// Set a key to a value and commit a graph version when graph state changes.
    pub fn changed_to<K: Key>(&self, key: K, value: K::Value) -> Result<GraphVersion, UpdateError> {
        let mut updater = self.updater();
        updater.changed_to(key, value)?;
        updater.commit()
    }

    /// Replace an injected value and commit a graph version when graph state changes.
    pub fn re_inject<K: InjectedKey>(
        &self,
        key: K,
        value: K::Value,
    ) -> Result<GraphVersion, UpdateError> {
        let mut updater = self.updater();
        updater.re_inject(key, value)?;
        updater.commit()
    }

    /// Drop graph state and reject snapshots outside the retained graph range.
    pub fn unstable_drop_everything(&self) -> GraphVersion {
        self.inner.graph.unstable_drop_everything()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use derive_more::Display;
    use tokio::{sync::Notify, time::timeout};

    use super::*;
    use crate::{ComputeCtx, DataStore};

    struct VersionedGateData {
        starts: AtomicUsize,
        started: Notify,
        release: Notify,
    }

    trait HasVersionedGateData {
        fn versioned_gate_data(&self) -> &Arc<VersionedGateData>;
    }

    impl HasVersionedGateData for DataStore {
        fn versioned_gate_data(&self) -> &Arc<VersionedGateData> {
            self.get::<Arc<VersionedGateData>>()
        }
    }

    #[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
    #[display("VersionedGate")]
    struct VersionedGate;

    impl Key for VersionedGate {
        type Value = u64;

        async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
            let data = ctx.global_data().versioned_gate_data();
            data.starts.fetch_add(1, Ordering::SeqCst);
            data.started.notify_waiters();
            data.release.notified().await;
            ctx.graph_version().as_u64()
        }
    }

    async fn wait_for_starts(data: &VersionedGateData, expected: usize) {
        timeout(Duration::from_secs(1), async {
            loop {
                if data.starts.load(Ordering::SeqCst) >= expected {
                    break;
                }
                data.started.notified().await;
            }
        })
        .await
        .expect("compute start count did not reach expected value");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_flight_computes_are_scoped_to_snapshot_epoch() {
        let data = Arc::new(VersionedGateData {
            starts: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        });
        let engine = ComputeEngine::builder().with_data(data.clone()).build();

        let snapshot_zero = engine.snapshot();
        let task_zero =
            tokio::spawn(async move { snapshot_zero.compute(&VersionedGate).await.unwrap() });
        wait_for_starts(&data, 1).await;

        engine.inner.graph.advance_for_tests();
        let snapshot_one = engine.snapshot();
        let task_one =
            tokio::spawn(async move { snapshot_one.compute(&VersionedGate).await.unwrap() });
        wait_for_starts(&data, 2).await;

        data.release.notify_waiters();
        assert_eq!(task_zero.await.unwrap(), 0);
        assert_eq!(task_one.await.unwrap(), 1);
    }
}

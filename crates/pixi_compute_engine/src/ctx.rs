//! The per-compute [`ComputeCtx`]. It is the channel through which a
//! running [`Key::compute`] requests dependencies.

use std::{collections::HashSet, future::Future, sync::Arc};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::{
    AnyKey, ComputeError, GraphVersion, Key, StorageType,
    cycle::{
        CycleError,
        active_edges::{ActiveEdges, DetectedCycle},
        guard::{GuardHandle, GuardStack},
    },
    engine::EngineInner,
    key_graph::{KeyFuture, Lookup, RecordedDeps, SpawnGeneration, boxed_compute_future},
    versions::VersionEpoch,
};

/// How [`ComputeCtx::resolve`] classified a `ctx.compute(...)` call.
enum Resolved<V> {
    /// Completed cache hit: value available immediately.
    Value(V),
    /// A spawned compute is in flight.
    Future {
        shared: crate::key_graph::ComputeFuture<V>,
        on_complete: Option<EdgeGuard>,
    },
    /// A dirty value is being validated through its recorded deps.
    CheckDeps {
        future: BoxFuture<'static, Result<V, ComputeError>>,
        on_complete: Option<EdgeGuard>,
    },
    /// The request closed a dependency cycle. Guards on the cycle
    /// path have already been notified; the caller should yield
    /// pending forever so that whichever guard's `select!` wins can
    /// drop this future.
    Cycle,
}

/// Shared dependency accumulator for a single compute frame.
type DepsList = Arc<Mutex<DepAccumulator>>;

type ParallelDepsGroup = Arc<Mutex<Vec<Option<RecordedDeps>>>>;

#[derive(Default)]
struct DepAccumulator {
    entries: Vec<DepEntry>,
}

enum DepEntry {
    Dep(RecordedDeps),
    Parallel(ParallelDepsGroup),
}

impl DepAccumulator {
    fn record_key(&mut self, key: AnyKey) {
        self.entries.push(DepEntry::Dep(RecordedDeps::key(key)));
    }

    fn record_parallel_group(&mut self, group: ParallelDepsGroup) {
        self.entries.push(DepEntry::Parallel(group));
    }

    fn finish(&mut self) -> RecordedDeps {
        let entries = std::mem::take(&mut self.entries);
        let deps = entries
            .into_iter()
            .map(|entry| match entry {
                DepEntry::Dep(dep) => dep,
                DepEntry::Parallel(group) => {
                    let deps = group.lock().iter().filter_map(Clone::clone).collect();
                    RecordedDeps::parallel(deps)
                }
            })
            .collect();
        RecordedDeps::serial(deps)
    }
}

/// Context passed to [`Key::compute`] so it can request dependencies.
///
/// The ctx is the only API a Key's compute body has for talking to the
/// engine. It carries:
///
/// - a handle to the engine's shared state (dedup cache, global data,
///   active-edge graph),
/// - the currently-running key's identity, used as the source endpoint
///   of any dependency edge added by `ctx.compute(..)`,
/// - the shared dep accumulator that records every `ctx.compute(..)`
///   call made in this compute frame (including parallel sub-ctxes),
/// - the branch-local cycle guard stack that
///   [`with_cycle_guard`](Self::with_cycle_guard) pushes onto for
///   scoped cycle recovery.
///
/// # `&mut self` on `compute`
///
/// Calling [`ComputeCtx::compute`] takes `&mut self`, which forces
/// dependency requests within a single compute frame to be serialized.
/// That is intentional: dependency recording mutates the shared dep
/// accumulator, and `&mut self` rules out the races that would make
/// the recorded order non-deterministic. Deterministic ordering
/// matters because introspection and (future) invalidation rely on a
/// stable, reproducible dep list for each key. For explicit parallel
/// dependency requests, use one of the parallel combinators below.
///
/// # Parallel combinators
///
/// The engine offers a fixed-arity and a variable-arity family, with
/// `Result`-aware variants for each:
///
/// | Shape                  | Plain               | Try (short-circuits)      |
/// |------------------------|---------------------|---------------------------|
/// | two branches           | [`compute2`]        | [`try_compute2`]          |
/// | three branches         | [`compute3`]        | [`try_compute3`]          |
/// | N items mapped         | [`compute_join`]    | [`try_compute_join`]      |
/// | N hand-built futures   | [`compute_many`]    | (caller joins)            |
///
/// Each closure receives its own fresh `&mut ComputeCtx` sub-ctx that
/// shares the parent's active-edge state and dep accumulator, and
/// chains its own branch-local guard stack to the parent's so cycle
/// detection keeps working across the parallel split.
///
/// # Combinator closure shape
///
/// All parallel combinators take closures of the form
/// `for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> T + Send`. The
/// idiomatic body is a direct compute call:
///
/// ```
/// # use std::fmt;
/// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
/// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
/// # struct Leaf(u32);
/// # impl fmt::Display for Leaf {
/// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
/// # }
/// # impl Key for Leaf {
/// #     type Value = u32;
/// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value { self.0 }
/// # }
/// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
/// # struct Sum;
/// # impl fmt::Display for Sum {
/// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "Sum") }
/// # }
/// impl Key for Sum {
///     type Value = u32;
///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
///         let (a, b) = ctx
///             .compute2(
///                 async |ctx| ctx.compute(&Leaf(1)).await,
///                 async |ctx| ctx.compute(&Leaf(2)).await,
///             )
///             .await;
///         a + b
///     }
/// }
/// # tokio_test::block_on(async {
/// #     let engine = ComputeEngine::new();
/// #     assert_eq!(engine.compute(&Sum).await.unwrap(), 3);
/// # });
/// ```
///
/// When the closure is produced by [`Iterator::map`] or stored in a
/// `let` binding, the `for<'x>` binder often fails to infer; pin it
/// with [`declare_closure`] / [`declare_join_closure`].
///
/// # Cycle handling
///
/// [`ComputeCtx::compute`] returns the child's [`Value`](crate::Key::Value)
/// directly, without a `Result` wrapper. Cycles are detected
/// synchronously inside `ctx.compute`:
///
/// - If a [`with_cycle_guard`](Self::with_cycle_guard) scope on the
///   cycle path encloses the call, its `Err(CycleError)` branch
///   fires and the cycling future is dropped.
/// - Otherwise the cycle surfaces at
///   [`ComputeEngine::compute`](crate::ComputeEngine::compute) as
///   [`Err(ComputeError::Cycle)`](crate::ComputeError::Cycle)
///   carrying the full ring of keys.
///
/// [`compute2`]: Self::compute2
/// [`compute3`]: Self::compute3
/// [`try_compute2`]: Self::try_compute2
/// [`try_compute3`]: Self::try_compute3
/// [`compute_many`]: Self::compute_many
/// [`compute_join`]: Self::compute_join
/// [`try_compute_join`]: Self::try_compute_join
/// [`declare_closure`]: Self::declare_closure
/// [`declare_join_closure`]: Self::declare_join_closure
pub struct ComputeCtx {
    engine: Arc<EngineInner>,
    version: GraphVersion,
    epoch: VersionEpoch,
    /// The Key whose compute is currently running in this frame, or
    /// `None` for the root ctx inside
    /// [`ComputeEngine::compute`](crate::ComputeEngine::compute) before
    /// it dispatches into a Key.
    ///
    /// Used as the source endpoint of any dependency edge added when
    /// this ctx calls [`Self::compute`].
    current: Option<AnyKey>,
    /// Dependencies recorded by the currently-computing key. Shared
    /// across parallel sub-ctxes so every branch contributes to the
    /// same set; flushed into the `Completed` node when the parent's
    /// compute body returns.
    deps: DepsList,
    /// Stack of active [`with_cycle_guard`](Self::with_cycle_guard)
    /// scopes in this compute frame. Parallel sub-ctxes (from
    /// [`compute2`](Self::compute2) and friends) each get a fresh
    /// branch stack chained to this one via the parent link, so a
    /// branch's pushes stay local while outer guards installed
    /// before the parallel split and the task's synthetic fallback
    /// at the root remain visible via `innermost`/`fallback`
    /// lookups up the chain.
    guard_stack: Arc<GuardStack>,
}

impl ComputeCtx {
    pub(crate) fn new(
        engine: Arc<EngineInner>,
        version: GraphVersion,
        epoch: VersionEpoch,
    ) -> Self {
        Self {
            engine,
            version,
            epoch,
            current: None,
            deps: Arc::new(Mutex::new(DepAccumulator::default())),
            guard_stack: Arc::new(GuardStack::new()),
        }
    }

    /// Build a root ctx paired with a synthetic cycle fallback so a
    /// user-facing scope (see
    /// [`ComputeEngine::with_ctx`](crate::ComputeEngine::with_ctx))
    /// can surface transitive cycles instead of parking forever when
    /// an awaited dep fails with a cycle.
    pub(crate) fn new_root_with_fallback(
        engine: Arc<EngineInner>,
        version: GraphVersion,
        epoch: VersionEpoch,
    ) -> (Self, oneshot::Receiver<CycleError>) {
        let (tx, rx) = oneshot::channel();
        let guard_stack = Arc::new(GuardStack::new());
        guard_stack.set_fallback(Arc::new(GuardHandle::new(tx)));
        let ctx = Self {
            engine,
            version,
            epoch,
            current: None,
            deps: Arc::new(Mutex::new(DepAccumulator::default())),
            guard_stack,
        };
        (ctx, rx)
    }

    /// Access the engine-wide shared data store.
    ///
    /// Values are set at engine construction time via
    /// [`ComputeEngineBuilder::with_data`](crate::ComputeEngineBuilder::with_data)
    /// and are immutable for the engine's lifetime. Downstream crates
    /// typically define extension traits on [`DataStore`](crate::DataStore)
    /// for ergonomic access:
    ///
    /// ```ignore
    /// let gw = ctx.global_data().gateway();
    /// ```
    pub fn global_data(&self) -> &crate::DataStore {
        &self.engine.global_data
    }

    /// The graph version associated with this context.
    pub fn graph_version(&self) -> GraphVersion {
        self.version
    }

    /// Build a [`ParallelBuilder`] that mints one future per parallel
    /// branch, each owning its own sub-ctx that shares this ctx's
    /// active-edge state and dep accumulator but carries a
    /// branch-local cycle-guard stack.
    ///
    /// Use this when the set of branches is dynamic (a walk that
    /// discovers new work as earlier branches complete). For a fixed
    /// arity / iterator of branches, prefer the wrapper combinators:
    /// [`compute2`](Self::compute2), [`compute3`](Self::compute3),
    /// [`compute_many`](Self::compute_many),
    /// [`compute_join`](Self::compute_join),
    /// [`try_compute_join`](Self::try_compute_join).
    pub fn parallel(&mut self) -> ParallelBuilder<'_> {
        let group = Arc::new(Mutex::new(Vec::new()));
        self.deps.lock().record_parallel_group(group.clone());
        let inner = if self.engine.sequential_branches {
            ParallelBuilderInner::Serial {
                engine: &self.engine,
                version: self.version,
                epoch: self.epoch,
                current: &self.current,
                guard_stack: &self.guard_stack,
                prev_done: None,
            }
        } else {
            ParallelBuilderInner::Concurrent {
                engine: &self.engine,
                version: self.version,
                epoch: self.epoch,
                current: &self.current,
                guard_stack: &self.guard_stack,
            }
        };
        ParallelBuilder { inner, group }
    }

    /// Request the value of another Key as a dependency of the
    /// currently running compute.
    ///
    /// `ctx.compute(&K).await` returns the child's
    /// [`Value`](crate::Key::Value) directly. The first request for
    /// a given key spawns its compute on a tokio task; subsequent
    /// requests for the same key dedup onto that task (or read the
    /// cached value if it already completed). The dependency is
    /// also recorded on this frame's dep list so introspection can
    /// see the graph.
    ///
    /// The returned future is precisely-captured (`use<K>`): it does
    /// not borrow `key`, so callers can pass a temporary reference
    /// such as `ctx.compute(&Fib(n - 1))` without lifetime issues.
    ///
    /// # Cycles
    ///
    /// If this call would close a dependency cycle, and a
    /// [`with_cycle_guard`](Self::with_cycle_guard) scope on the
    /// cycle path encloses the call, it is delivered to that scope
    /// as `Err(CycleError)`. Otherwise the cycle surfaces at
    /// [`ComputeEngine::compute`](crate::ComputeEngine::compute) as
    /// [`Err(ComputeError::Cycle)`](crate::ComputeError::Cycle)
    /// carrying the full ring of keys.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Base(u32);
    /// # impl fmt::Display for Base {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// # impl Key for Base {
    /// #     type Value = u32;
    /// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value { self.0 }
    /// # }
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Double(u32);
    /// # impl fmt::Display for Double {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// impl Key for Double {
    ///     type Value = u32;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         let base = ctx.compute(&Base(self.0)).await;
    ///         base * 2
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     assert_eq!(engine.compute(&Double(21)).await.unwrap(), 42);
    /// # });
    /// ```
    pub fn compute<K: Key>(&mut self, key: &K) -> impl Future<Output = K::Value> + use<K> {
        let resolved = self.resolve(key);
        let guard_stack = self.guard_stack.clone();
        async move { Self::await_dependency(resolved, guard_stack).await }
    }

    /// Same as [`compute`](Self::compute) but returns a
    /// `Result<K::Value, ComputeError>` for use at the engine
    /// boundary. Internal; called by
    /// [`ComputeEngine::compute`](crate::ComputeEngine::compute),
    /// which runs outside any compute body and therefore cannot
    /// panic on `Canceled` without burning the caller's engine
    /// handle.
    pub(crate) fn compute_root<K: Key>(
        &mut self,
        key: &K,
    ) -> impl Future<Output = Result<K::Value, ComputeError>> + use<K> {
        let resolved = self.resolve(key);
        async move { Self::await_root_dependency(resolved).await }
    }

    /// Run `f` with a cycle guard installed. If a cycle closes on
    /// this key's cycle path while the scope is active, the guard
    /// fires and this call returns `Err(CycleError)` with the ring.
    ///
    /// Without a guard on this key, a cycle surfaces at
    /// [`ComputeEngine::compute`](crate::ComputeEngine::compute) as
    /// [`Err(ComputeError::Cycle)`](crate::ComputeError::Cycle).
    /// Opting in via `with_cycle_guard` is the only way for a
    /// compute body to recover from a cycle as a domain error.
    ///
    /// Guards are strict: a scope catches only cycles whose path
    /// contains the guarding key. A cycle that happens deeper in
    /// the dependency graph, below a `ctx.compute(&X)` the caller
    /// wrapped in a guard, is not rewound into the caller's scope.
    /// See the crate-level `# Cycles` docs for the rationale.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Node(u32);
    /// # impl fmt::Display for Node {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// impl Key for Node {
    ///     // Users fold cycle recovery into the Value.
    ///     type Value = Result<u32, String>;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         // A self-loop, guarded. Without the guard,
    ///         // `engine.compute(&Node(_))` would return
    ///         // `Err(ComputeError::Cycle)`.
    ///         let me = self.0;
    ///         ctx.with_cycle_guard(async |ctx| ctx.compute(&Node(me)).await)
    ///             .await
    ///             .unwrap_or_else(|cycle| Err(format!("cycle at Node({me}): {cycle}")))
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     let caught = engine.compute(&Node(7)).await.unwrap().unwrap_err();
    /// #     assert!(caught.starts_with("cycle at Node(7)"));
    /// # });
    /// ```
    pub async fn with_cycle_guard<'s, F, T>(&'s mut self, f: F) -> Result<T, CycleError>
    where
        F: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> T + Send,
    {
        let (tx, rx) = oneshot::channel();
        let handle = Arc::new(GuardHandle::new(tx));
        self.guard_stack.push(handle.clone());
        let _frame = GuardFrame::new(Arc::clone(&self.guard_stack), handle);

        // The select lets the cycle detector preempt the user's
        // future: when the detector fires this guard's sender, the
        // `rx` arm wins and the racing future is dropped, which is
        // what breaks the deadlock the cycle would otherwise cause.
        //
        // `biased;` ensures the cycle arm wins even in the race
        // where the inner future also completes in the same poll
        // (for example, the inner future was awaiting a shared
        // that the detector just fired the cycle for). Without it
        // the random pick would occasionally let the scope return
        // `Ok(value)` despite having been notified.
        let future = f(self);
        let result = tokio::select! {
            biased;
            cycle = rx => match cycle {
                Ok(err) => Err(err),
                // The sender lives in the guard frame, and the
                // frame is held on the stack for the whole scope,
                // so this case is structurally impossible.
                Err(_) => unreachable!(
                    "cycle guard sender dropped while scope is still active"
                ),
            },
            value = future => Ok(value),
        };

        result
    }

    /// Synchronous core of [`compute`](Self::compute): cycle check,
    /// graph lookup/spawn, and dep recording.
    fn resolve<K: Key>(&mut self, key: &K) -> Resolved<K::Value> {
        let any_key = AnyKey::new(key.clone());

        // Cheap path first: peek the per-type slot without inserting on
        // miss. Installing the active edge holds a single global
        // parking_lot mutex and runs a BFS of the entire active-edge
        // graph; under fan-out (80+ concurrent compute callers all
        // reading the same InjectedKey) that combination serializes
        // everyone behind one lock and adds tens of seconds of wall
        // time per call. For an already-completed value there is
        // nothing to await, so no edge is needed.
        if let Some(value) = self.engine.graph.lookup_value(key.clone(), self.version) {
            self.deps.lock().record_key(any_key);
            return Resolved::Value(value);
        }

        // Miss or in-flight: we are going to wait. Install the active
        // edge before spawning the child task so that any descendant
        // task that races ahead and tries to close a back-edge sees
        // our edge already in place.
        let edge_guard = match self.current.clone() {
            Some(caller) => match self.install_active_edge(caller, &any_key) {
                Ok(edge) => Some(edge),
                Err(detected) => {
                    Self::notify_cycle(detected);
                    self.deps.lock().record_key(any_key);
                    return Resolved::Cycle;
                }
            },
            // Root ctx: no caller identity, nothing to add to the
            // edge graph, nothing to cycle-check against.
            None => None,
        };

        if K::storage_type() == StorageType::Injected {
            let hit = self
                .engine
                .graph
                .lookup(key.clone(), self.version, self.epoch);
            self.deps.lock().record_key(any_key.clone());
            return match hit {
                Some(Lookup::Completed(value)) => {
                    drop(edge_guard);
                    Resolved::Value(value)
                }
                Some(Lookup::CheckDeps {
                    value,
                    deps,
                    verified_from,
                }) => Resolved::CheckDeps {
                    future: check_deps_or_compute::<K>(
                        self.engine.clone(),
                        self.version,
                        self.epoch,
                        key.clone(),
                        value,
                        deps,
                        verified_from,
                    ),
                    on_complete: edge_guard,
                },
                Some(Lookup::InFlight(shared)) => Resolved::Future {
                    shared,
                    on_complete: edge_guard,
                },
                None => panic!(
                    "injected key not set: {}. All injected values must be provided via ComputeEngine::inject() before computing keys that depend on them.",
                    any_key,
                ),
            };
        }

        let child_current = Some(any_key.clone());
        let prepared = prepare_compute_future::<K>(
            self.engine.clone(),
            self.version,
            self.epoch,
            key.clone(),
            child_current,
        );
        let lookup = self.engine.graph.get_or_insert_with(
            key.clone(),
            self.version,
            self.epoch,
            move |generation| prepared.spawn(generation),
        );

        self.deps.lock().record_key(any_key);

        match lookup {
            // Lost a benign race between the peek and the insert:
            // another caller just completed it. Drop the edge guard
            // (the BFS work was paid but harmless) and return the
            // cached value.
            Lookup::Completed(value) => {
                drop(edge_guard);
                Resolved::Value(value)
            }
            Lookup::CheckDeps {
                value,
                deps,
                verified_from,
            } => Resolved::CheckDeps {
                future: check_deps_or_compute::<K>(
                    self.engine.clone(),
                    self.version,
                    self.epoch,
                    key.clone(),
                    value,
                    deps,
                    verified_from,
                ),
                on_complete: edge_guard,
            },
            Lookup::InFlight(shared) => Resolved::Future {
                shared,
                on_complete: edge_guard,
            },
        }
    }

    /// Install the active wait edge `caller -> target`.
    ///
    /// The atomic `try_add` check is required for correctness under
    /// concurrent detection: see `ActiveEdges` for the TOCTOU failure mode a
    /// non-atomic check-then-add would allow. The edge captures the notify
    /// target visible from this branch-local guard stack at edge-creation
    /// time, so cycle routing never reconsults a mutable stack at detection
    /// time.
    fn install_active_edge(
        &self,
        caller: AnyKey,
        target: &AnyKey,
    ) -> Result<EdgeGuard, DetectedCycle> {
        let notify = self
            .guard_stack
            .innermost()
            .expect("spawned task carries a synthetic fallback");
        self.engine
            .active_edges
            .try_add(&caller, target, notify)
            .map(|id| EdgeGuard {
                active_edges: self.engine.active_edges.clone(),
                from: caller,
                to: target.clone(),
                id,
            })
    }

    async fn await_dependency<V: Clone>(resolved: Resolved<V>, guard_stack: Arc<GuardStack>) -> V {
        match resolved {
            Resolved::Value(value) => value,
            Resolved::Future {
                shared,
                on_complete,
            } => {
                let result = shared.await;
                drop(on_complete);
                Self::unwrap_dependency_result(result, guard_stack).await
            }
            Resolved::CheckDeps {
                future,
                on_complete,
            } => {
                let result = future.await;
                drop(on_complete);
                Self::unwrap_dependency_result(result, guard_stack).await
            }
            Resolved::Cycle => Self::pending_after_direct_cycle().await,
        }
    }

    async fn await_root_dependency<V: Clone>(resolved: Resolved<V>) -> Result<V, ComputeError> {
        match resolved {
            Resolved::Value(value) => Ok(value),
            Resolved::Future {
                shared,
                on_complete,
            } => {
                let result = shared.await;
                drop(on_complete);
                result
            }
            Resolved::CheckDeps {
                future,
                on_complete,
            } => {
                let result = future.await;
                drop(on_complete);
                result
            }
            Resolved::Cycle => std::future::pending().await,
        }
    }

    async fn unwrap_dependency_result<V: Clone>(
        result: Result<V, ComputeError>,
        guard_stack: Arc<GuardStack>,
    ) -> V {
        match result {
            Ok(value) => value,
            Err(ComputeError::Canceled) => {
                panic!("compute was canceled while a caller was awaiting it")
            }
            Err(ComputeError::Rejected) => {
                panic!("compute graph version was rejected while a caller was awaiting it")
            }
            Err(ComputeError::Cycle(err)) => {
                Self::park_after_transitive_cycle(guard_stack, err).await
            }
        }
    }

    async fn park_after_transitive_cycle<V>(guard_stack: Arc<GuardStack>, err: CycleError) -> V {
        // The awaited task ended because of a cycle our key does not participate
        // in. Route directly to our task's synthetic fallback so a user
        // `with_cycle_guard` on this key is not fired for a cycle it is not on.
        if let Some(guard) = guard_stack.fallback() {
            guard.notify(err);
        }
        std::future::pending().await
    }

    async fn pending_after_direct_cycle<V>() -> V {
        // Yielding pending lets the enclosing guard's `select!` win its cycle arm
        // and drop this future. Returning or panicking would either require
        // synthesizing a `Value` or bypass the requested guard-based recovery.
        std::future::pending().await
    }

    fn notify_cycle(detected: DetectedCycle) {
        let err = CycleError {
            path: detected.path,
        };
        // Dedup by `Arc` identity: a single scope may have created multiple edges
        // in the ring, and each `notify` is a `take()`-once oneshot.
        let mut seen: HashSet<*const GuardHandle> = HashSet::new();
        for target in &detected.targets {
            if seen.insert(Arc::as_ptr(target)) {
                target.notify(err.clone());
            }
        }
    }

    /// Run two closures concurrently, each with its own sub-ctx,
    /// and return their two values as a tuple.
    ///
    /// Both branches contribute to the same parent's dep list and
    /// share the parent's active-edge state, so cycle detection
    /// keeps working across the split. Each branch gets a fresh
    /// branch-local cycle guard stack chained to the parent's, so
    /// a `with_cycle_guard` opened inside one branch cannot leak
    /// into a sibling.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Leaf(u32);
    /// # impl fmt::Display for Leaf {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// # impl Key for Leaf {
    /// #     type Value = u32;
    /// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value { self.0 }
    /// # }
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Pair;
    /// # impl fmt::Display for Pair {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "Pair") }
    /// # }
    /// impl Key for Pair {
    ///     type Value = u32;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         let (a, b) = ctx
    ///             .compute2(
    ///                 async |ctx| ctx.compute(&Leaf(1)).await,
    ///                 async |ctx| ctx.compute(&Leaf(2)).await,
    ///             )
    ///             .await;
    ///         a + b
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     assert_eq!(engine.compute(&Pair).await.unwrap(), 3);
    /// # });
    /// ```
    pub async fn compute2<C1, T, C2, U>(&mut self, c1: C1, c2: C2) -> (T, U)
    where
        C1: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> T + Send,
        C2: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> U + Send,
    {
        let mut p = self.parallel();
        futures::future::join(p.compute(c1), p.compute(c2)).await
    }

    /// Run three closures concurrently. Same semantics as
    /// [`compute2`](Self::compute2), arity three. For more than
    /// three branches use [`compute_many`](Self::compute_many) or
    /// [`compute_join`](Self::compute_join).
    pub async fn compute3<C1, T, C2, U, C3, V>(&mut self, c1: C1, c2: C2, c3: C3) -> (T, U, V)
    where
        C1: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> T + Send,
        C2: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> U + Send,
        C3: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> V + Send,
    {
        let mut p = self.parallel();
        futures::future::join3(p.compute(c1), p.compute(c2), p.compute(c3)).await
    }

    /// `Result`-aware variant of [`compute2`](Self::compute2): each
    /// branch produces a `Result<_, E>`, and the join short-circuits
    /// on the first `Err` (the other branch is dropped).
    ///
    /// Useful for a pair of sub-computes that each return a domain
    /// error in their `Value` and where the caller wants to fail
    /// fast rather than run the other branch to completion.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Parse(&'static str);
    /// # impl fmt::Display for Parse {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// # impl Key for Parse {
    /// #     type Value = Result<u32, String>;
    /// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value {
    /// #         self.0.parse().map_err(|e: std::num::ParseIntError| e.to_string())
    /// #     }
    /// # }
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct SumPair;
    /// # impl fmt::Display for SumPair {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "SumPair") }
    /// # }
    /// impl Key for SumPair {
    ///     type Value = Result<u32, String>;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         let (a, b) = ctx
    ///             .try_compute2(
    ///                 async |ctx| ctx.compute(&Parse("10")).await,
    ///                 async |ctx| ctx.compute(&Parse("oops")).await,
    ///             )
    ///             .await?;
    ///         Ok(a + b)
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     let err = engine.compute(&SumPair).await.unwrap().unwrap_err();
    /// #     assert!(err.contains("invalid digit"));
    /// # });
    /// ```
    pub async fn try_compute2<C1, T, C2, U, E>(&mut self, c1: C1, c2: C2) -> Result<(T, U), E>
    where
        C1: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> Result<T, E> + Send,
        C2: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> Result<U, E> + Send,
    {
        let mut p = self.parallel();
        futures::future::try_join(p.compute(c1), p.compute(c2)).await
    }

    /// `Result`-aware variant of [`compute3`](Self::compute3);
    /// short-circuits on the first `Err`. See
    /// [`try_compute2`](Self::try_compute2) for usage.
    pub async fn try_compute3<C1, T, C2, U, C3, V, E>(
        &mut self,
        c1: C1,
        c2: C2,
        c3: C3,
    ) -> Result<(T, U, V), E>
    where
        C1: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> Result<T, E> + Send,
        C2: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> Result<U, E> + Send,
        C3: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> Result<V, E> + Send,
    {
        let mut p = self.parallel();
        futures::future::try_join3(p.compute(c1), p.compute(c2), p.compute(c3)).await
    }

    /// Build a vector of compute futures, one per input closure,
    /// each owning its own sub-ctx. Returned synchronously, so the
    /// caller chooses how to join them (for example
    /// `futures::future::join_all`, or picking a subset).
    ///
    /// Use this when you need fine control over the concurrency
    /// shape, for example to race a subset of sub-computes or to
    /// feed the futures into a custom scheduler. If you just want
    /// a "join all items mapped to a compute", reach for
    /// [`compute_join`](Self::compute_join) instead.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use futures::future::join_all;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Leaf(u32);
    /// # impl fmt::Display for Leaf {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// # impl Key for Leaf {
    /// #     type Value = u32;
    /// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value { self.0 }
    /// # }
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Sum;
    /// # impl fmt::Display for Sum {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "Sum") }
    /// # }
    /// impl Key for Sum {
    ///     type Value = u32;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         let futs = ctx.compute_many((1..=3).map(|n| {
    ///             ComputeCtx::declare_closure(async move |ctx: &mut ComputeCtx| {
    ///                 ctx.compute(&Leaf(n)).await
    ///             })
    ///         }));
    ///         join_all(futs).await.into_iter().sum()
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     assert_eq!(engine.compute(&Sum).await.unwrap(), 6);
    /// # });
    /// ```
    pub fn compute_many<Items, F, T>(
        &mut self,
        computes: Items,
    ) -> Vec<impl Future<Output = T> + use<Items, F, T>>
    where
        Items: IntoIterator<Item = F>,
        F: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> T + Send,
    {
        let mut p = self.parallel();
        computes.into_iter().map(|func| p.compute(func)).collect()
    }

    /// Map each input item to a compute via `mapper` and join the
    /// resulting futures concurrently into a `Vec` of their values.
    ///
    /// `mapper` is an [`AsyncFn`] applied once per item; capture
    /// per-item data via the closure's parameter, not via the
    /// environment.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Square(u32);
    /// # impl fmt::Display for Square {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// # impl Key for Square {
    /// #     type Value = u32;
    /// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value { self.0 * self.0 }
    /// # }
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Squares;
    /// # impl fmt::Display for Squares {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "Squares") }
    /// # }
    /// impl Key for Squares {
    ///     type Value = Vec<u32>;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         ctx.compute_join(1..=4u32, async |ctx, n| ctx.compute(&Square(n)).await)
    ///             .await
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     assert_eq!(engine.compute(&Squares).await.unwrap(), vec![1, 4, 9, 16]);
    /// # });
    /// ```
    pub async fn compute_join<Items, Mapper, T, R>(
        &mut self,
        items: Items,
        mapper: Mapper,
    ) -> Vec<R>
    where
        Items: IntoIterator<Item = T>,
        Mapper: for<'x> AsyncFn(&'x mut ComputeCtx, T) -> R + Send + Clone,
        T: Send,
    {
        let mut p = self.parallel();
        futures::future::join_all(items.into_iter().map(|item| {
            let mapper = mapper.clone();
            p.compute(async move |ctx: &mut ComputeCtx| mapper(ctx, item).await)
        }))
        .await
    }

    /// Pin the HRTB binder on a single-argument compute closure.
    ///
    /// When a closure is built inline at the call site, Rust's
    /// inference usually picks the `for<'x> AsyncFnOnce(&'x mut
    /// ComputeCtx) -> T` bound without help. When the closure
    /// flows through an adapter like [`Iterator::map`], a `let`
    /// binding, or a type-erasing collection, inference sometimes
    /// fails to universally quantify over the ctx lifetime.
    /// Wrapping the closure in `declare_closure` is a no-op at
    /// runtime that re-asserts the bound so inference succeeds.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use futures::future::join_all;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Leaf(u32);
    /// # impl fmt::Display for Leaf {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// # impl Key for Leaf {
    /// #     type Value = u32;
    /// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value { self.0 }
    /// # }
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Total;
    /// # impl fmt::Display for Total {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "Total") }
    /// # }
    /// impl Key for Total {
    ///     type Value = u32;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         // Built via `Iterator::map`. Without `declare_closure`,
    ///         // the inner closure's HRTB would fail to infer.
    ///         let futs = ctx.compute_many((1..=3).map(|n| {
    ///             ComputeCtx::declare_closure(async move |ctx: &mut ComputeCtx| {
    ///                 ctx.compute(&Leaf(n)).await
    ///             })
    ///         }));
    ///         join_all(futs).await.into_iter().sum()
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     assert_eq!(engine.compute(&Total).await.unwrap(), 6);
    /// # });
    /// ```
    pub fn declare_closure<F, T>(f: F) -> F
    where
        F: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> T + Send,
    {
        f
    }

    /// Pin the HRTB binder on a two-argument join-mapper closure.
    ///
    /// Same purpose as [`declare_closure`](Self::declare_closure)
    /// but for the `AsyncFn(&mut ComputeCtx, item) -> R` shape
    /// taken by [`compute_join`](Self::compute_join) and
    /// [`try_compute_join`](Self::try_compute_join). Useful when
    /// the mapper is stored in a `let` binding before being
    /// passed in.
    pub fn declare_join_closure<M, T, R>(m: M) -> M
    where
        M: for<'x> AsyncFn(&'x mut ComputeCtx, T) -> R + Send + Clone,
    {
        m
    }

    /// `Result`-aware variant of [`compute_join`](Self::compute_join):
    /// each mapped future produces a `Result<_, E>`, and the join
    /// short-circuits on the first `Err`.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::fmt;
    /// # use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct Parse(String);
    /// # impl fmt::Display for Parse {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
    /// # }
    /// # impl Key for Parse {
    /// #     type Value = Result<u32, String>;
    /// #     async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value {
    /// #         self.0.parse().map_err(|e: std::num::ParseIntError| e.to_string())
    /// #     }
    /// # }
    /// # #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    /// # struct ParseAll;
    /// # impl fmt::Display for ParseAll {
    /// #     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "ParseAll") }
    /// # }
    /// impl Key for ParseAll {
    ///     type Value = Result<Vec<u32>, String>;
    ///     async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
    ///         let inputs = ["1", "2", "three"].map(String::from);
    ///         ctx.try_compute_join(inputs, async |ctx, s| ctx.compute(&Parse(s)).await)
    ///             .await
    ///     }
    /// }
    /// # tokio_test::block_on(async {
    /// #     let engine = ComputeEngine::new();
    /// #     let err = engine.compute(&ParseAll).await.unwrap().unwrap_err();
    /// #     assert!(err.contains("invalid digit"));
    /// # });
    /// ```
    pub async fn try_compute_join<Items, Mapper, T, R, E>(
        &mut self,
        items: Items,
        mapper: Mapper,
    ) -> Result<Vec<R>, E>
    where
        Items: IntoIterator<Item = T>,
        Mapper: for<'x> AsyncFn(&'x mut ComputeCtx, T) -> Result<R, E> + Send + Clone,
        T: Send,
    {
        let mut p = self.parallel();
        futures::future::try_join_all(items.into_iter().map(|item| {
            let mapper = mapper.clone();
            p.compute(async move |ctx: &mut ComputeCtx| mapper(ctx, item).await)
        }))
        .await
    }
}

/// RAII removal of one user cycle-guard frame.
///
/// Removal is by identity rather than "pop top" because branch-local guard
/// stacks can still contain overlapping nested scopes in panic/unwind paths.
struct GuardFrame {
    stack: Arc<GuardStack>,
    handle: Arc<GuardHandle>,
}

impl GuardFrame {
    fn new(stack: Arc<GuardStack>, handle: Arc<GuardHandle>) -> Self {
        Self { stack, handle }
    }
}

impl Drop for GuardFrame {
    fn drop(&mut self) {
        self.stack.remove(&self.handle);
    }
}

/// RAII: removes an active compute→compute edge when the caller's
/// await resumes. Dropped either when the shared future resolves or
/// when the caller's future is itself dropped.
struct EdgeGuard {
    active_edges: Arc<ActiveEdges>,
    from: AnyKey,
    to: AnyKey,
    /// Identity of the specific edge record we installed. Used so
    /// we only remove our own record, not any sibling parallel
    /// waits on the same `(from, to)` pair.
    id: crate::cycle::active_edges::EdgeId,
}

impl Drop for EdgeGuard {
    fn drop(&mut self) {
        self.active_edges.remove(&self.from, &self.to, self.id);
    }
}

/// Handle returned by [`ComputeCtx::parallel`] for minting one future
/// per parallel branch.
///
/// Use this when the set of branches is not known up front (e.g. a
/// dynamic walk that discovers new work as earlier branches complete).
/// Hold a `ParallelBuilder` across the walk and call
/// [`compute`](Self::compute) per push to mint a future that runs its
/// body in a fresh sub-[`ComputeCtx`] with its own branch-local cycle
/// guard stack, so sibling branches cannot disturb each other's
/// `with_cycle_guard` scopes. The returned futures are independent
/// and can be driven through a `FuturesUnordered` or similar.
///
/// For a fixed set of branches, prefer [`ComputeCtx::compute2`],
/// [`ComputeCtx::compute_join`], [`ComputeCtx::try_compute_join`], or
/// [`ComputeCtx::compute_many`] — they wrap this builder for common
/// cases.
pub struct ParallelBuilder<'p> {
    inner: ParallelBuilderInner<'p>,
    group: ParallelDepsGroup,
}

enum ParallelBuilderInner<'p> {
    Concurrent {
        engine: &'p Arc<EngineInner>,
        version: GraphVersion,
        epoch: VersionEpoch,
        current: &'p Option<AnyKey>,
        guard_stack: &'p Arc<GuardStack>,
    },
    Serial {
        engine: &'p Arc<EngineInner>,
        version: GraphVersion,
        epoch: VersionEpoch,
        current: &'p Option<AnyKey>,
        guard_stack: &'p Arc<GuardStack>,
        prev_done: Option<oneshot::Receiver<()>>,
    },
}

impl ParallelBuilder<'_> {
    /// Mint one branch future. `func` runs on a fresh sub-[`ComputeCtx`]
    /// with a branch-local cycle-guard stack chained to the parent's.
    ///
    /// The returned future is precisely-captured (`+ use<F, T>`) and
    /// does not borrow from `self`, so multiple branch futures can be
    /// awaited concurrently (e.g. via `futures::future::join_all` or
    /// a `FuturesUnordered`).
    pub fn compute<F, T>(&mut self, func: F) -> impl Future<Output = T> + use<F, T>
    where
        F: for<'x> AsyncFnOnce(&'x mut ComputeCtx) -> T + Send,
    {
        let (engine, version, epoch, current, guard_stack) = match &self.inner {
            ParallelBuilderInner::Concurrent {
                engine,
                version,
                epoch,
                current,
                guard_stack,
                ..
            }
            | ParallelBuilderInner::Serial {
                engine,
                version,
                epoch,
                current,
                guard_stack,
                ..
            } => (*engine, *version, *epoch, *current, *guard_stack),
        };
        let branch_index = {
            let mut group = self.group.lock();
            let index = group.len();
            group.push(None);
            index
        };
        let branch_group = self.group.clone();
        let branch_deps = Arc::new(Mutex::new(DepAccumulator::default()));
        // Branch-local guard stack: a fresh stack that chains to
        // the parent's stack. Pushes and removes inside this
        // branch's `with_cycle_guard` scopes stay local, so
        // concurrent sibling branches cannot see or disturb each
        // other's frames. Lookups via `innermost`/`fallback` walk
        // up the parent chain to reach outer guards and the task's
        // synthetic fallback at the root.
        let mut ctx = ComputeCtx {
            engine: engine.clone(),
            version,
            epoch,
            current: current.clone(),
            deps: branch_deps.clone(),
            guard_stack: Arc::new(GuardStack::new_branch(guard_stack.clone())),
        };
        let (prev, done_tx) = match &mut self.inner {
            ParallelBuilderInner::Concurrent { .. } => (None, None),
            ParallelBuilderInner::Serial { prev_done, .. } => {
                let (tx, rx) = oneshot::channel();
                (prev_done.replace(rx), Some(tx))
            }
        };
        async move {
            if let Some(prev) = prev {
                let _ = prev.await;
            }
            let value = func(&mut ctx).await;
            let deps = branch_deps.lock().finish();
            branch_group.lock()[branch_index] = Some(deps);
            if let Some(done_tx) = done_tx {
                let _ = done_tx.send(());
            }
            value
        }
    }
}

fn check_deps_or_compute<K: Key>(
    engine: Arc<EngineInner>,
    version: GraphVersion,
    epoch: VersionEpoch,
    key: K,
    value: K::Value,
    deps: RecordedDeps,
    verified_from: GraphVersion,
) -> BoxFuture<'static, Result<K::Value, ComputeError>> {
    Box::pin(async move {
        if !check_recorded_deps(engine.clone(), version, epoch, deps.clone(), verified_from).await?
        {
            return compute_uncached::<K>(engine, version, epoch, key).await;
        }

        engine.graph.mark_unchanged::<K>(
            key.clone(),
            value.clone(),
            deps,
            verified_from,
            version,
            epoch,
        )?;
        Ok(value)
    })
}

fn check_recorded_deps(
    engine: Arc<EngineInner>,
    version: GraphVersion,
    epoch: VersionEpoch,
    deps: RecordedDeps,
    verified_from: GraphVersion,
) -> BoxFuture<'static, Result<bool, ComputeError>> {
    Box::pin(async move {
        match deps {
            RecordedDeps::Empty => Ok(true),
            RecordedDeps::Key(dep) => {
                dep.compute_for_check_deps(engine.clone(), version, epoch)
                    .await?;
                Ok(engine
                    .graph
                    .deps_unchanged_since(vec![dep], verified_from, version))
            }
            RecordedDeps::Serial(deps) => {
                for dep in deps {
                    if !check_recorded_deps(engine.clone(), version, epoch, dep, verified_from)
                        .await?
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            RecordedDeps::Parallel(deps) => {
                let checks = deps.into_iter().map(|dep| {
                    check_recorded_deps(engine.clone(), version, epoch, dep, verified_from)
                });
                let results = futures::future::join_all(checks).await;
                let mut unchanged = true;
                for result in results {
                    unchanged &= result?;
                }
                Ok(unchanged)
            }
        }
    })
}

async fn compute_uncached<K: Key>(
    engine: Arc<EngineInner>,
    version: GraphVersion,
    epoch: VersionEpoch,
    key: K,
) -> Result<K::Value, ComputeError> {
    let guard_stack = Arc::new(GuardStack::new());
    let (fallback_tx, fallback_rx) = oneshot::channel();
    guard_stack.set_fallback(Arc::new(GuardHandle::new(fallback_tx)));

    let current = Some(AnyKey::new(key.clone()));
    let mut child_ctx = ComputeCtx {
        engine: engine.clone(),
        version,
        epoch,
        current,
        deps: Arc::new(Mutex::new(DepAccumulator::default())),
        guard_stack,
    };

    let key_for_body = key.clone();
    let engine_for_body = engine.clone();
    let compute_body = async move {
        let value = key_for_body.compute(&mut child_ctx).await;
        let final_deps = child_ctx.deps.lock().finish();
        engine_for_body.graph.insert_computed_direct::<K>(
            key_for_body.clone(),
            value.clone(),
            final_deps,
            version,
            epoch,
        )?;
        Ok(value)
    };

    tokio::select! {
        biased;
        cycle = fallback_rx => Err(ComputeError::Cycle(
            cycle.expect("synthetic cycle fallback sender dropped"),
        )),
        value = compute_body => value,
    }
}

struct PreparedCompute<K: Key> {
    runtime: tokio::runtime::Handle,
    start_tx: oneshot::Sender<SpawnGeneration>,
    result_rx: oneshot::Receiver<Result<K::Value, ComputeError>>,
    wrapped: BoxFuture<'static, ()>,
}

impl<K: Key> PreparedCompute<K> {
    fn spawn(self, generation: SpawnGeneration) -> KeyFuture<K> {
        let handle = self.runtime.spawn(self.wrapped);
        let _ = self.start_tx.send(generation);
        boxed_compute_future(handle, self.result_rx)
    }
}

fn prepare_compute_future<K: Key>(
    engine: Arc<EngineInner>,
    version: GraphVersion,
    epoch: VersionEpoch,
    key: K,
    current: Option<AnyKey>,
) -> PreparedCompute<K> {
    let runtime = tokio::runtime::Handle::current();
    let (start_tx, start_rx) = oneshot::channel::<SpawnGeneration>();
    let (result_tx, result_rx) = oneshot::channel::<Result<K::Value, ComputeError>>();

    let engine_for_inner = engine.clone();
    let body: BoxFuture<'static, ()> = Box::pin(async move {
        let Ok(generation) = start_rx.await else {
            let _ = result_tx.send(Err(ComputeError::Canceled));
            return;
        };

        let guard_stack = Arc::new(GuardStack::new());
        let (fallback_tx, fallback_rx) = oneshot::channel();
        guard_stack.set_fallback(Arc::new(GuardHandle::new(fallback_tx)));

        let mut child_ctx = ComputeCtx {
            engine: engine_for_inner.clone(),
            version,
            epoch,
            current,
            deps: Arc::new(Mutex::new(DepAccumulator::default())),
            guard_stack,
        };

        let engine_for_body = engine_for_inner.clone();
        let key_for_body = key.clone();
        let compute_body = async move {
            let value = key_for_body.compute(&mut child_ctx).await;
            let final_deps = child_ctx.deps.lock().finish();
            engine_for_body.graph.insert_completed::<K>(
                key_for_body.clone(),
                value.clone(),
                final_deps,
                version,
                epoch,
                generation,
            )?;
            Ok(value)
        };

        let result: Result<K::Value, ComputeError> = tokio::select! {
            biased;
            cycle = fallback_rx => Err(ComputeError::Cycle(
                cycle.expect("synthetic cycle fallback sender dropped"),
            )),
            value = compute_body => value,
        };

        let _ = result_tx.send(result);
    });

    let wrapped = match engine.spawn_hook.as_ref() {
        Some(hook) => hook.wrap(&engine.global_data, body),
        None => body,
    };

    PreparedCompute {
        runtime,
        start_tx,
        result_rx,
        wrapped,
    }
}

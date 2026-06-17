//! The engine's in-memory key store. Doubles as the dependency graph:
//! each entry carries either an in-flight compute future or a completed
//! value plus its recorded dependencies, so introspection iterates the
//! same data structure that drives caching.
//!
//! The store is sharded by Key type. For each concrete `K: Key` it
//! holds a [`PerTypeSlot<K>`] containing a map of `K → GraphNode<K>`
//! with two states:
//!
//! - `InFlight`: a weak reference to the `Shared` future of a
//!   currently-running compute. Subscribers hold the corresponding
//!   strong `Shared` clones; when the last subscriber drops, the weak
//!   reference becomes dangling and the spawned task is aborted by the
//!   `AbortOnDrop` guard wrapping its `JoinHandle`.
//! - `Completed`: a value whose compute ran to completion together
//!   with the deps observed during that compute. The spawned task
//!   transitions the entry into `Completed` synchronously as its last
//!   step before returning, so any value that lands here was produced
//!   by a task that was never aborted.
//!
//! Type erasure across the outer map goes through the [`TypedSlot`]
//! trait: typed access (insert / lookup) downcasts via [`TypedSlot::as_any`],
//! type-erased iteration (introspection) goes through [`TypedSlot::snapshot`].

use std::{
    any::{Any, TypeId},
    collections::{HashMap, HashSet},
    sync::Arc,
    thread,
};

use futures::{
    FutureExt,
    future::{BoxFuture, Shared, WeakShared},
};
use parking_lot::Mutex;

use crate::{
    AnyKey, ComputeError, Key, StorageType,
    versions::{GraphVersion, VersionEpoch},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum RecordedDeps {
    #[default]
    Empty,
    Key(AnyKey),
    Serial(Vec<RecordedDeps>),
    Parallel(Vec<RecordedDeps>),
}

impl RecordedDeps {
    pub(crate) fn key(key: AnyKey) -> Self {
        Self::Key(key)
    }

    pub(crate) fn serial(deps: Vec<RecordedDeps>) -> Self {
        Self::from_group(deps, Self::Serial)
    }

    pub(crate) fn parallel(deps: Vec<RecordedDeps>) -> Self {
        Self::from_group(deps, Self::Parallel)
    }

    fn from_group(
        deps: Vec<RecordedDeps>,
        build: impl FnOnce(Vec<RecordedDeps>) -> RecordedDeps,
    ) -> Self {
        let deps: Vec<_> = deps.into_iter().filter(|dep| !dep.is_empty()).collect();
        match deps.len() {
            0 => Self::Empty,
            1 => deps.into_iter().next().expect("len checked"),
            _ => build(deps),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    pub(crate) fn flattened(&self) -> Vec<AnyKey> {
        let mut out = Vec::new();
        self.flatten_into(&mut out);
        out
    }

    fn flatten_into(&self, out: &mut Vec<AnyKey>) {
        match self {
            Self::Empty => {}
            Self::Key(key) => out.push(key.clone()),
            Self::Serial(deps) | Self::Parallel(deps) => {
                for dep in deps {
                    dep.flatten_into(out);
                }
            }
        }
    }
}

/// A boxed compute future for Key `K`.
pub(crate) type KeyFuture<K> = BoxFuture<'static, Result<<K as Key>::Value, ComputeError>>;

/// The shared future produced by a single compute.
pub(crate) type ComputeFuture<V> = Shared<BoxFuture<'static, Result<V, ComputeError>>>;

/// Result of looking up a Key in the store.
pub(crate) enum Lookup<V> {
    /// A completed value is available immediately.
    Completed(V),
    /// An invalidated value is present with its recorded deps. The
    /// caller validates those deps at the requested graph version
    /// before deciding whether to reuse this value.
    CheckDeps {
        value: V,
        deps: RecordedDeps,
        verified_from: GraphVersion,
    },
    /// A shared computation is running; await it to receive the value.
    InFlight(ComputeFuture<V>),
}

/// Identity of a single spawn within a [`PerTypeSlot`].
///
/// A monotonically increasing per-slot counter, handed out at
/// `InFlight` install time and threaded into the spawned task. The
/// completion path uses it to verify that the slot still holds *this*
/// task's `InFlight` entry before promoting to `Completed` (rather
/// than blindly overwriting whatever is there, which would clobber a
/// fresh re-spawn that landed in the slot after the original task was
/// cancelled but before its post-`.await` completion path ran).
pub(crate) type SpawnGeneration = u64;

/// One stored value in a [`PerTypeSlot`].
pub(crate) enum GraphNode<K: Key> {
    /// Computed values keyed by graph version.
    Completed {
        versions: Vec<CompletedEntry<K::Value>>,
    },
    /// Values injected via [`ComputeEngine::inject`](crate::ComputeEngine::inject)
    /// or changed through graph updates.
    Injected {
        versions: Vec<InjectedEntry<K::Value>>,
    },
}

pub(crate) struct CompletedEntry<V> {
    valid_from: GraphVersion,
    value: V,
    deps: RecordedDeps,
}

pub(crate) struct InjectedEntry<V> {
    valid_from: GraphVersion,
    value: V,
}

fn prune_versioned_entries<T>(
    entries: &mut Vec<T>,
    retention_floor: GraphVersion,
    valid_from: impl Fn(&T) -> GraphVersion,
) {
    let first_at_or_after = entries
        .iter()
        .position(|entry| valid_from(entry) >= retention_floor);
    match first_at_or_after {
        Some(0) => {}
        Some(index) if valid_from(&entries[index]) == retention_floor => {
            entries.drain(0..index);
        }
        Some(index) => {
            entries.drain(0..index - 1);
        }
        None if entries.len() > 1 => {
            let last = entries.pop().expect("len checked");
            entries.clear();
            entries.push(last);
        }
        None => {}
    }
}

struct InFlightNode<K: Key> {
    future: WeakShared<KeyFuture<K>>,
    generation: SpawnGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirtyKind {
    ForceDirty,
    Invalidated,
}

#[derive(Clone, Copy, Debug)]
struct DirtyEntry {
    version: GraphVersion,
    kind: DirtyKind,
}

/// Compact graph identity used by dependency metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct GraphKey(u32);

/// Interns erased keys to stable graph identities.
#[derive(Default)]
struct KeyInterner {
    by_key: HashMap<AnyKey, GraphKey>,
    keys: Vec<AnyKey>,
}

impl KeyInterner {
    fn len(&self) -> usize {
        self.keys.len()
    }

    fn index(&mut self, key: AnyKey) -> GraphKey {
        if let Some(index) = self.by_key.get(&key) {
            return *index;
        }

        let index = GraphKey(
            self.keys
                .len()
                .try_into()
                .expect("graph key index overflow"),
        );
        self.keys.push(key.clone());
        self.by_key.insert(key, index);
        index
    }
}

/// Cached state for a single concrete Key type.
pub(crate) struct PerTypeSlot<K: Key> {
    pub(crate) nodes: HashMap<K, GraphNode<K>>,
    in_flight: HashMap<(K, GraphVersion, VersionEpoch), InFlightNode<K>>,
    /// Source for [`SpawnGeneration`] tokens. Increments on every
    /// in-flight install.
    next_generation: SpawnGeneration,
}

impl<K: Key> PerTypeSlot<K> {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            in_flight: HashMap::new(),
            next_generation: 0,
        }
    }

    /// Try to satisfy a lookup from this slot. Stale in-flight
    /// entries (last subscriber dropped, task aborted) are removed as
    /// a side effect.
    fn lookup(
        &mut self,
        key: &K,
        version: GraphVersion,
        epoch: VersionEpoch,
        dirty_at: Option<DirtyEntry>,
    ) -> Option<Lookup<K::Value>> {
        let in_flight_key = (key.clone(), version, epoch);
        if let Some(task) = self.in_flight.get(&in_flight_key) {
            if let Some(shared) = task.future.upgrade() {
                return Some(Lookup::InFlight(shared));
            }
            self.in_flight.remove(&in_flight_key);
        }

        self.lookup_stored(key, version, dirty_at)
    }

    fn lookup_stored(
        &self,
        key: &K,
        version: GraphVersion,
        dirty_at: Option<DirtyEntry>,
    ) -> Option<Lookup<K::Value>> {
        match self.nodes.get(key) {
            Some(GraphNode::Completed { versions }) => {
                let entry = versions
                    .iter()
                    .rev()
                    .find(|entry| entry.valid_from <= version)?;
                match dirty_at {
                    Some(dirty) if entry.valid_from < dirty.version => {
                        if dirty.kind == DirtyKind::Invalidated && !entry.deps.is_empty() {
                            Some(Lookup::CheckDeps {
                                value: entry.value.clone(),
                                deps: entry.deps.clone(),
                                verified_from: entry.valid_from,
                            })
                        } else {
                            None
                        }
                    }
                    _ => Some(Lookup::Completed(entry.value.clone())),
                }
            }
            Some(GraphNode::Injected { versions }) => versions
                .iter()
                .rev()
                .find(|entry| entry.valid_from <= version)
                .map(|entry| Lookup::Completed(entry.value.clone())),
            None => None,
        }
    }

    fn lookup_value(
        &self,
        key: &K,
        version: GraphVersion,
        dirty_at: Option<DirtyEntry>,
    ) -> Option<K::Value> {
        match self.lookup_stored(key, version, dirty_at) {
            Some(Lookup::Completed(value)) => Some(value),
            Some(Lookup::CheckDeps { .. } | Lookup::InFlight(_)) | None => None,
        }
    }
}

/// Erased view of a per-type slot. Carries both a typed-downcast
/// escape hatch ([`as_any`](TypedSlot::as_any)) for the insert/lookup
/// hot path and a type-erased snapshot method for introspection.
pub(crate) trait TypedSlot: Send + Sync + 'static {
    fn as_any(&self) -> &dyn Any;
    fn snapshot(&self, out: &mut Vec<NodeRecord>);
    fn stats(&self, out: &mut KeyGraphStats);
    fn remove_in_flight_for(&self, version: GraphVersion, epoch: VersionEpoch) -> usize;
    fn prune_history(&self, retention_floor: GraphVersion);
}

/// A single per-node record produced by [`TypedSlot::snapshot`]. The
/// introspection layer turns these into its own, public-facing
/// representation.
pub(crate) struct NodeRecord {
    pub key: AnyKey,
    pub state: RawNodeState,
    pub deps: Vec<AnyKey>,
}

pub(crate) enum RawNodeState {
    Computing,
    Completed,
    Injected,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyGraphStats {
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

impl<K: Key> TypedSlot for Mutex<PerTypeSlot<K>> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn snapshot(&self, out: &mut Vec<NodeRecord>) {
        let g = self.lock();
        let mut running = HashSet::new();
        for ((key, _, _), task) in &g.in_flight {
            if task.future.upgrade().is_some() && running.insert(key.clone()) {
                out.push(NodeRecord {
                    key: AnyKey::new(key.clone()),
                    state: RawNodeState::Computing,
                    deps: Vec::new(),
                });
            }
        }
        for (key, node) in &g.nodes {
            if running.contains(key) {
                continue;
            }
            let any_key = AnyKey::new(key.clone());
            match node {
                GraphNode::Completed { versions } => {
                    if let Some(entry) = versions.last() {
                        out.push(NodeRecord {
                            key: any_key,
                            state: RawNodeState::Completed,
                            deps: entry.deps.flattened(),
                        });
                    }
                }
                GraphNode::Injected { .. } => {
                    out.push(NodeRecord {
                        key: any_key,
                        state: RawNodeState::Injected,
                        deps: Vec::new(),
                    });
                }
            }
        }
    }

    fn stats(&self, out: &mut KeyGraphStats) {
        let g = self.lock();
        out.node_count += g.nodes.len();
        for node in g.nodes.values() {
            match node {
                GraphNode::Completed { versions } => {
                    out.completed_nodes += 1;
                    out.completed_versions += versions.len();
                }
                GraphNode::Injected { versions } => {
                    out.injected_nodes += 1;
                    out.injected_versions += versions.len();
                }
            }
        }
        for task in g.in_flight.values() {
            if task.future.upgrade().is_some() {
                out.in_flight_count += 1;
            } else {
                out.stale_in_flight_count += 1;
            }
        }
    }

    fn remove_in_flight_for(&self, version: GraphVersion, epoch: VersionEpoch) -> usize {
        let mut g = self.lock();
        let before = g.in_flight.len();
        g.in_flight.retain(|(_, stored_version, stored_epoch), _| {
            *stored_version != version || *stored_epoch != epoch
        });
        before - g.in_flight.len()
    }

    fn prune_history(&self, retention_floor: GraphVersion) {
        let mut g = self.lock();
        for node in g.nodes.values_mut() {
            match node {
                GraphNode::Completed { versions } => {
                    prune_versioned_entries(versions, retention_floor, |entry| entry.valid_from)
                }
                GraphNode::Injected { versions } => {
                    prune_versioned_entries(versions, retention_floor, |entry| entry.valid_from)
                }
            }
        }
    }
}

/// The engine's keyed graph storage.
///
/// Access is through a single outer [`Mutex`] guarding the type-id
/// map. The outer lock is held only long enough to look up or insert
/// the per-type slot's `Arc`; per-type ops then take the inner per-type
/// `Mutex` independently. No `.await` happens under either lock.
pub(crate) struct KeyGraph {
    inner: Mutex<HashMap<TypeId, Arc<dyn TypedSlot>>>,
    key_interner: Mutex<KeyInterner>,
    /// Dirty markers keyed by interned graph identity. Direct
    /// invalidations are force-dirty; reverse-dependency propagation
    /// records invalidated markers that can be checked through recorded
    /// deps.
    dirty: Mutex<HashMap<GraphKey, Vec<DirtyEntry>>>,
    /// Equality/backdating ranges keyed by interned graph identity. A
    /// range records that a key has the same value and deps at both
    /// bounds.
    unchanged: Mutex<HashMap<GraphKey, Vec<(GraphVersion, GraphVersion)>>>,
    /// Reverse dependency index used only for dirty propagation.
    rdeps: Mutex<HashMap<GraphKey, HashSet<GraphKey>>>,
}

impl KeyGraph {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            key_interner: Mutex::new(KeyInterner::default()),
            dirty: Mutex::new(HashMap::new()),
            unchanged: Mutex::new(HashMap::new()),
            rdeps: Mutex::new(HashMap::new()),
        }
    }

    /// Look up `key` in the graph. If the value is already cached
    /// (completed, injected, or in-flight), returns it immediately.
    ///
    /// On a miss, behavior depends on [`Key::storage_type`]:
    ///
    /// **Computed** keys: mints a fresh [`SpawnGeneration`], calls
    /// `make_future` (which must be quick because the per-type lock is
    /// held; a slow closure blocks every other caller for this key
    /// type), installs the resulting future as `InFlight`, and returns
    /// the strong `Shared`.
    ///
    /// **Injected** keys: panics. Initial injected values must be
    /// provided via [`insert_injected`](Self::insert_injected) before
    /// any compute that depends on them; later replacements go through
    /// graph updates so dependents are dirtied.
    pub(crate) fn get_or_insert_with<K, F>(
        &self,
        key: &K,
        version: GraphVersion,
        epoch: VersionEpoch,
        make_future: F,
    ) -> Lookup<K::Value>
    where
        K: Key,
        F: FnOnce(SpawnGeneration) -> KeyFuture<K>,
    {
        let graph_key = self.index_key(AnyKey::new(key.clone()));
        let dirty_at = self.dirty_at_for(graph_key, version);
        let slot = self.get_or_create_slot::<K>();
        let typed: &Mutex<PerTypeSlot<K>> = slot
            .as_any()
            .downcast_ref()
            .expect("type id matches by construction");
        let mut s = typed.lock();
        if let Some(hit) = s.lookup(key, version, epoch, dirty_at) {
            return hit;
        }

        // Miss. For injected keys this means the value was never
        // provided; for computed keys we spawn a fresh task.
        match K::storage_type() {
            StorageType::Injected => {
                panic!(
                    "injected key not set: {}. \
                     All injected values must be provided via \
                     ComputeEngine::inject() before computing keys \
                     that depend on them.",
                    AnyKey::new(key.clone()),
                );
            }
            StorageType::Computed => {
                let generation = s.next_generation;
                s.next_generation += 1;
                let shared = make_future(generation).shared();
                let weak = shared
                    .downgrade()
                    .expect("freshly-created Shared must be downgradeable");
                s.in_flight.insert(
                    (key.clone(), version, epoch),
                    InFlightNode {
                        future: weak,
                        generation,
                    },
                );
                Lookup::InFlight(shared)
            }
        }
    }

    /// Promote a freshly-computed value into the slot if it still
    /// holds the `InFlight` entry that was installed for this spawn
    /// (matched by [`SpawnGeneration`]). If the slot has moved on
    /// (a fresh re-spawn replaced our entry after our subscribers
    /// dropped, or a different task already promoted the value), the
    /// computed value is silently dropped: the live entry is
    /// authoritative.
    ///
    /// Called by the spawned task as its last synchronous step before
    /// returning its value. Because the insert is sync and happens
    /// after the last `.await`, cancellation cannot interrupt it.
    pub(crate) fn insert_completed<K: Key>(
        &self,
        key: &K,
        value: K::Value,
        deps: RecordedDeps,
        version: GraphVersion,
        epoch: VersionEpoch,
        generation: SpawnGeneration,
    ) -> bool {
        let slot = self.get_or_create_slot::<K>();
        let typed: &Mutex<PerTypeSlot<K>> = slot
            .as_any()
            .downcast_ref()
            .expect("type id matches by construction");
        let mut s = typed.lock();
        let in_flight_key = (key.clone(), version, epoch);
        let owns_slot = matches!(
            s.in_flight.get(&in_flight_key),
            Some(task) if task.generation == generation,
        );
        if owns_slot {
            s.in_flight.remove(&in_flight_key);
            let parent = self.index_key(AnyKey::new(key.clone()));
            let previous = match s.nodes.get(key) {
                Some(GraphNode::Completed { versions }) => versions.last(),
                Some(GraphNode::Injected { .. }) => {
                    unreachable!("computed key cannot complete into an injected node")
                }
                None => None,
            };
            let old_deps = previous
                .map(|entry| entry.deps.flattened())
                .unwrap_or_default();
            let unchanged_from = previous.and_then(|entry| {
                (entry.deps == deps && K::equality(&entry.value, &value))
                    .then_some(entry.valid_from)
            });
            let new_deps = deps.flattened();
            let entry = CompletedEntry {
                valid_from: version,
                value,
                deps,
            };
            match s.nodes.get_mut(key) {
                Some(GraphNode::Completed { versions }) => {
                    if let Some(existing) = versions
                        .iter_mut()
                        .find(|existing| existing.valid_from == version)
                    {
                        *existing = entry;
                    } else {
                        versions.push(entry);
                        versions.sort_by_key(|entry| entry.valid_from);
                    }
                }
                None => {
                    s.nodes.insert(
                        key.clone(),
                        GraphNode::Completed {
                            versions: vec![entry],
                        },
                    );
                }
                Some(GraphNode::Injected { .. }) => {
                    unreachable!("computed key cannot complete into an injected node")
                }
            }
            drop(s);
            if let Some(from) = unchanged_from {
                self.record_unchanged(parent.clone(), from, version);
            }
            self.replace_rdeps(parent, old_deps, new_deps);
            true
        } else {
            false
        }
    }

    pub(crate) fn mark_unchanged<K: Key>(
        &self,
        key: &K,
        value: K::Value,
        deps: RecordedDeps,
        verified_from: GraphVersion,
        version: GraphVersion,
    ) {
        self.insert_computed_direct(key, value, deps, version);
        self.record_unchanged(
            self.index_key(AnyKey::new(key.clone())),
            verified_from,
            version,
        );
    }

    pub(crate) fn insert_computed_direct<K: Key>(
        &self,
        key: &K,
        value: K::Value,
        deps: RecordedDeps,
        version: GraphVersion,
    ) {
        let slot = self.get_or_create_slot::<K>();
        let typed: &Mutex<PerTypeSlot<K>> = slot
            .as_any()
            .downcast_ref()
            .expect("type id matches by construction");
        let mut s = typed.lock();
        let parent = self.index_key(AnyKey::new(key.clone()));
        let previous = match s.nodes.get(key) {
            Some(GraphNode::Completed { versions }) => versions.last(),
            Some(GraphNode::Injected { .. }) => {
                unreachable!("computed key cannot complete into an injected node")
            }
            None => None,
        };
        let old_deps = previous
            .map(|entry| entry.deps.flattened())
            .unwrap_or_default();
        let unchanged_from = previous.and_then(|entry| {
            (entry.deps == deps && K::equality(&entry.value, &value)).then_some(entry.valid_from)
        });
        let new_deps = deps.flattened();
        let entry = CompletedEntry {
            valid_from: version,
            value,
            deps,
        };
        match s.nodes.get_mut(key) {
            Some(GraphNode::Completed { versions }) => {
                if let Some(existing) = versions
                    .iter_mut()
                    .find(|existing| existing.valid_from == version)
                {
                    *existing = entry;
                } else {
                    versions.push(entry);
                    versions.sort_by_key(|entry| entry.valid_from);
                }
            }
            None => {
                s.nodes.insert(
                    key.clone(),
                    GraphNode::Completed {
                        versions: vec![entry],
                    },
                );
            }
            Some(GraphNode::Injected { .. }) => {
                unreachable!("computed key cannot complete into an injected node")
            }
        }
        drop(s);
        if let Some(from) = unchanged_from {
            self.record_unchanged(parent.clone(), from, version);
        }
        self.replace_rdeps(parent, old_deps, new_deps);
    }

    /// Look up `key` without creating an entry on miss. Used by the
    /// injected-key path in [`ComputeCtx::compute`](crate::ComputeCtx::compute)
    /// where spawning a compute is never appropriate.
    pub(crate) fn lookup<K: Key>(
        &self,
        key: &K,
        version: GraphVersion,
        epoch: VersionEpoch,
    ) -> Option<Lookup<K::Value>> {
        let map = self.inner.lock();
        let slot = map.get(&TypeId::of::<K>())?.clone();
        drop(map);
        let typed: &Mutex<PerTypeSlot<K>> = slot
            .as_any()
            .downcast_ref()
            .expect("type id matches by construction");
        let graph_key = self.index_key(AnyKey::new(key.clone()));
        let dirty_at = self.dirty_at_for(graph_key, version);
        let mut s = typed.lock();
        s.lookup(key, version, epoch, dirty_at)
    }

    pub(crate) fn lookup_value<K: Key>(&self, key: &K, version: GraphVersion) -> Option<K::Value> {
        let map = self.inner.lock();
        let slot = map.get(&TypeId::of::<K>())?.clone();
        drop(map);
        let typed: &Mutex<PerTypeSlot<K>> = slot
            .as_any()
            .downcast_ref()
            .expect("type id matches by construction");
        let graph_key = self.index_key(AnyKey::new(key.clone()));
        let dirty_at = self.dirty_at_for(graph_key, version);
        let s = typed.lock();
        s.lookup_value(key, version, dirty_at)
    }

    pub(crate) fn mark_dirty<K: Key>(&self, key: &K, version: GraphVersion) -> bool {
        if K::storage_type() == StorageType::Injected {
            panic!("injected key cannot be invalidated without a replacement value");
        }

        self.mark_dirty_cascade(self.index_key(AnyKey::new(key.clone())), version)
    }

    pub(crate) fn changed_to<K: Key>(
        &self,
        key: &K,
        value: K::Value,
        version: GraphVersion,
    ) -> bool {
        let graph_key = self.index_key(AnyKey::new(key.clone()));
        let slot = self.get_or_create_slot::<K>();
        let typed: &Mutex<PerTypeSlot<K>> = slot
            .as_any()
            .downcast_ref()
            .expect("type id matches by construction");
        let mut s = typed.lock();

        let mut changed = true;
        let mut old_deps = Vec::new();
        match s.nodes.get_mut(key) {
            Some(GraphNode::Injected { versions }) => {
                if versions
                    .last()
                    .is_some_and(|entry| K::equality(&entry.value, &value))
                {
                    changed = false;
                } else {
                    versions.push(InjectedEntry {
                        valid_from: version,
                        value,
                    });
                }
            }
            Some(GraphNode::Completed { versions }) => {
                if versions
                    .last()
                    .is_some_and(|entry| entry.deps.is_empty() && K::equality(&entry.value, &value))
                {
                    changed = false;
                } else {
                    old_deps = versions
                        .last()
                        .map(|entry| entry.deps.flattened())
                        .unwrap_or_default();
                    versions.push(CompletedEntry {
                        valid_from: version,
                        value,
                        deps: RecordedDeps::Empty,
                    });
                    versions.sort_by_key(|entry| entry.valid_from);
                }
            }
            None if K::storage_type() == StorageType::Injected => {
                s.nodes.insert(
                    key.clone(),
                    GraphNode::Injected {
                        versions: vec![InjectedEntry {
                            valid_from: version,
                            value,
                        }],
                    },
                );
            }
            None => {
                s.nodes.insert(
                    key.clone(),
                    GraphNode::Completed {
                        versions: vec![CompletedEntry {
                            valid_from: version,
                            value,
                            deps: RecordedDeps::Empty,
                        }],
                    },
                );
            }
        }
        drop(s);

        if changed {
            if !old_deps.is_empty() {
                self.replace_rdeps(graph_key, old_deps, Vec::new());
            }
            self.record_value_changed(graph_key, version);
            self.mark_rdeps_dirty(graph_key, version);
            true
        } else {
            false
        }
    }

    /// Store an injected value directly, without spawning a compute.
    ///
    /// # Panics
    ///
    /// Panics if `key` already has an entry in the graph (whether
    /// injected, completed, or in-flight). The single-write contract
    /// of [`InjectedKey`](crate::InjectedKey) means each key may be
    /// injected at most once.
    pub(crate) fn insert_injected<K: Key>(
        &self,
        key: &K,
        value: K::Value,
        version: GraphVersion,
    ) -> bool {
        let slot = self.get_or_create_slot::<K>();
        let typed: &Mutex<PerTypeSlot<K>> = slot
            .as_any()
            .downcast_ref()
            .expect("type id matches by construction");
        let mut s = typed.lock();
        if s.nodes.contains_key(key) || s.in_flight.keys().any(|(stored, _, _)| stored == key) {
            return false;
        }
        s.nodes.insert(
            key.clone(),
            GraphNode::Injected {
                versions: vec![InjectedEntry {
                    valid_from: version,
                    value,
                }],
            },
        );
        true
    }

    /// Clear all graph nodes and dependency metadata.
    pub(crate) fn clear(&self) {
        let old_inner = std::mem::take(&mut *self.inner.lock());
        let old_dirty = std::mem::take(&mut *self.dirty.lock());
        let old_unchanged = std::mem::take(&mut *self.unchanged.lock());
        let old_rdeps = std::mem::take(&mut *self.rdeps.lock());

        thread::Builder::new()
            .name("pixi-compute-graph-drop".to_owned())
            .spawn(move || drop((old_inner, old_dirty, old_unchanged, old_rdeps)))
            .expect("failed to spawn compute graph drop thread");
    }

    /// Walk every per-type slot, invoking `sink` with each one.
    /// Used by introspection to build a snapshot.
    pub(crate) fn for_each_slot(&self, mut sink: impl FnMut(&dyn TypedSlot)) {
        // Clone the Arcs out from under the outer lock so per-type
        // locks are taken without holding the outer one.
        let arcs: Vec<Arc<dyn TypedSlot>> = self.inner.lock().values().cloned().collect();
        for slot in &arcs {
            sink(&**slot);
        }
    }

    pub(crate) fn remove_in_flight_for(&self, version: GraphVersion, epoch: VersionEpoch) -> usize {
        let arcs: Vec<Arc<dyn TypedSlot>> = self.inner.lock().values().cloned().collect();
        arcs.iter()
            .map(|slot| slot.remove_in_flight_for(version, epoch))
            .sum()
    }

    pub(crate) fn prune_history(&self, retention_floor: GraphVersion) {
        let arcs: Vec<Arc<dyn TypedSlot>> = self.inner.lock().values().cloned().collect();
        for slot in arcs {
            slot.prune_history(retention_floor);
        }

        for entries in self.dirty.lock().values_mut() {
            prune_versioned_entries(entries, retention_floor, |entry| entry.version);
        }
        self.unchanged.lock().retain(|_, ranges| {
            ranges.retain(|(_, to)| *to >= retention_floor);
            !ranges.is_empty()
        });
    }

    pub(crate) fn stats(&self) -> KeyGraphStats {
        let interned_keys = self.key_interner.lock().len();
        let (dirty_keys, dirty_entries) = {
            let dirty = self.dirty.lock();
            (dirty.len(), dirty.values().map(Vec::len).sum())
        };
        let (rdep_keys, rdep_edges) = {
            let rdeps = self.rdeps.lock();
            (rdeps.len(), rdeps.values().map(HashSet::len).sum())
        };
        let (unchanged_keys, unchanged_ranges) = {
            let unchanged = self.unchanged.lock();
            (unchanged.len(), unchanged.values().map(Vec::len).sum())
        };

        let mut stats = KeyGraphStats {
            interned_keys,
            dirty_keys,
            dirty_entries,
            rdep_keys,
            rdep_edges,
            unchanged_keys,
            unchanged_ranges,
            ..KeyGraphStats::default()
        };
        self.for_each_slot(|slot| slot.stats(&mut stats));
        stats
    }

    fn index_key(&self, key: AnyKey) -> GraphKey {
        self.key_interner.lock().index(key)
    }

    /// Return true when every recorded dep is valid across the range
    /// being checked for a dirty parent.
    pub(crate) fn deps_unchanged_since(
        &self,
        deps: &[AnyKey],
        verified_from: GraphVersion,
        version: GraphVersion,
    ) -> bool {
        deps.iter().all(|dep| {
            let dep = self.index_key(dep.clone());
            self.key_unchanged_since(dep, verified_from, version)
        })
    }

    fn dirty_at_for(&self, key: GraphKey, version: GraphVersion) -> Option<DirtyEntry> {
        self.dirty.lock().get(&key).and_then(|versions| {
            versions
                .iter()
                .rev()
                .find(|dirty| dirty.version <= version)
                .copied()
        })
    }

    fn key_unchanged_since(
        &self,
        key: GraphKey,
        verified_from: GraphVersion,
        version: GraphVersion,
    ) -> bool {
        let dirty_after_verified = self
            .dirty
            .lock()
            .get(&key)
            .map(|versions| {
                versions
                    .iter()
                    .any(|dirty| verified_from < dirty.version && dirty.version <= version)
            })
            .unwrap_or(false);
        if !dirty_after_verified {
            return true;
        }

        self.unchanged.lock().get(&key).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|(from, to)| *from <= verified_from && version <= *to)
        })
    }

    /// Mark a directly changed key force-dirty, then mark all drained
    /// reverse deps as invalidated.
    fn mark_dirty_cascade(&self, key: GraphKey, version: GraphVersion) -> bool {
        let mut changed = false;
        let mut queue = self.mark_dirty_one(key, version, DirtyKind::ForceDirty, &mut changed);
        while let Some(key) = queue.pop() {
            queue.extend(self.mark_dirty_one(key, version, DirtyKind::Invalidated, &mut changed));
        }
        changed
    }

    /// Record that the key's own value changes at this version. The
    /// key remains directly readable through its typed node storage.
    fn record_value_changed(&self, key: GraphKey, version: GraphVersion) {
        let mut dirty = self.dirty.lock();
        let versions = dirty.entry(key).or_default();
        match versions.last_mut() {
            Some(last) if last.version == version => last.kind = DirtyKind::ForceDirty,
            _ => versions.push(DirtyEntry {
                version,
                kind: DirtyKind::ForceDirty,
            }),
        }
    }

    /// Mark only reverse deps as invalidated after a key value changes.
    fn mark_rdeps_dirty(&self, key: GraphKey, version: GraphVersion) -> bool {
        let mut changed = false;
        let mut queue: Vec<_> = self
            .rdeps
            .lock()
            .remove(&key)
            .map(|deps| deps.into_iter().collect())
            .unwrap_or_default();
        while let Some(key) = queue.pop() {
            queue.extend(self.mark_dirty_one(key, version, DirtyKind::Invalidated, &mut changed));
        }
        changed
    }

    fn mark_dirty_one(
        &self,
        key: GraphKey,
        version: GraphVersion,
        kind: DirtyKind,
        changed: &mut bool,
    ) -> Vec<GraphKey> {
        let inserted = {
            let mut dirty = self.dirty.lock();
            let versions = dirty.entry(key).or_default();
            match versions.last_mut() {
                Some(last) if last.version == version && last.kind == kind => false,
                Some(last) if last.version == version && last.kind == DirtyKind::ForceDirty => {
                    false
                }
                Some(last) if last.version == version => {
                    last.kind = kind;
                    true
                }
                _ => {
                    versions.push(DirtyEntry { version, kind });
                    true
                }
            }
        };

        if inserted {
            *changed = true;
            self.rdeps
                .lock()
                .remove(&key)
                .map(|deps| deps.into_iter().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Record an equality-based range for a recomputed key.
    fn record_unchanged(&self, key: GraphKey, from: GraphVersion, to: GraphVersion) {
        self.unchanged
            .lock()
            .entry(key)
            .or_default()
            .push((from, to));
    }

    fn replace_rdeps(&self, parent: GraphKey, old_deps: Vec<AnyKey>, new_deps: Vec<AnyKey>) {
        let old_deps: Vec<_> = old_deps
            .into_iter()
            .map(|dep| self.index_key(dep))
            .collect();
        let new_deps: Vec<_> = new_deps
            .into_iter()
            .map(|dep| self.index_key(dep))
            .collect();
        let mut rdeps = self.rdeps.lock();
        for dep in old_deps {
            if let Some(parents) = rdeps.get_mut(&dep) {
                parents.remove(&parent);
                if parents.is_empty() {
                    rdeps.remove(&dep);
                }
            }
        }
        for dep in new_deps {
            rdeps.entry(dep).or_default().insert(parent);
        }
    }

    fn get_or_create_slot<K: Key>(&self) -> Arc<dyn TypedSlot> {
        let mut map = self.inner.lock();
        map.entry(TypeId::of::<K>())
            .or_insert_with(|| Arc::new(Mutex::new(PerTypeSlot::<K>::new())) as Arc<dyn TypedSlot>)
            .clone()
    }
}

impl Default for KeyGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use derive_more::Display;

    use super::*;
    use crate::ComputeCtx;

    #[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
    #[display("{}", _0)]
    struct IndexedKey(u32);

    impl Key for IndexedKey {
        type Value = u32;

        async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value {
            self.0
        }
    }

    #[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
    #[display("{}", _0)]
    struct OtherIndexedKey(u32);

    impl Key for OtherIndexedKey {
        type Value = u32;

        async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value {
            self.0
        }
    }

    #[test]
    fn graph_key_interner_reuses_id_for_equal_keys() {
        let mut interner = KeyInterner::default();
        let first = interner.index(AnyKey::new(IndexedKey(1)));
        let second = interner.index(AnyKey::new(IndexedKey(1)));
        assert_eq!(first, second);
    }

    #[test]
    fn graph_key_interner_separates_distinct_key_types() {
        let mut interner = KeyInterner::default();
        let first = interner.index(AnyKey::new(IndexedKey(1)));
        let second = interner.index(AnyKey::new(OtherIndexedKey(1)));
        assert_ne!(first, second);
    }

    #[test]
    fn graph_clear_preserves_interned_key_ids() {
        let graph = KeyGraph::new();
        let first = graph.index_key(AnyKey::new(IndexedKey(1)));
        let second = graph.index_key(AnyKey::new(IndexedKey(2)));

        graph.clear();

        assert_eq!(second, graph.index_key(AnyKey::new(IndexedKey(2))));
        assert_ne!(first, graph.index_key(AnyKey::new(IndexedKey(2))));
    }
}

/// Wrap a spawned `JoinHandle` into a compute future that converts a
/// cancellation `JoinError` into [`ComputeError::Canceled`] and
/// propagates panics. The typed result is carried out of band via
/// `result_rx` so the spawned task itself can stay `()`-valued (so it
/// can be wrapped by a [`SpawnHook`](crate::SpawnHook) that does not
/// know the compute's `Value` type). The task's own `Result` is passed
/// through unchanged (so a task that ended via its outer cycle guard
/// can surface [`ComputeError::Cycle`] to its awaiter).
pub(crate) fn boxed_compute_future<V: Send + 'static>(
    handle: tokio::task::JoinHandle<()>,
    result_rx: tokio::sync::oneshot::Receiver<Result<V, ComputeError>>,
) -> BoxFuture<'static, Result<V, ComputeError>> {
    use crate::abort_on_drop::AbortOnDrop;

    async move {
        match AbortOnDrop(handle).await {
            Ok(()) => match result_rx.await {
                Ok(result) => result,
                // Task finished without sending: only happens if the
                // task was dropped between its final send point and
                // completion, which we treat as cancellation.
                Err(_) => Err(ComputeError::Canceled),
            },
            Err(e) if e.is_cancelled() => Err(ComputeError::Canceled),
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
    .boxed()
}

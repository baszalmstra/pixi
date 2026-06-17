# Pixi Compute-Engine Invalidation: DICE-style Mini-Design

Status: updated after plan grilling. The earlier removal-based invalidation plan is superseded.

The daemon requirement is:

- concurrent requests against the same workspace;
- concurrent filesystem monitoring that can commit graph changes while requests run;
- request-level graph consistency;
- no requirement for true historical OS filesystem snapshots.

Therefore Pixi should stay close to Buck2 DICE rather than Salsa's "writes block snapshots" model or a simple remove-from-cache invalidation model.

## 1. Chosen semantics

### 1.1 Graph-version snapshots

A request obtains a read-only compute snapshot/transaction at the latest committed graph version.

```text
request A starts at v10
filesystem watcher commits changed PathMetadataKey(foo) at v11
request B starts at v11
request A continues at v10
```

Snapshots do not expose invalidation or reinjection APIs. Only the master engine / graph owner can commit changes.

### 1.2 What the guarantee is

The guarantee is **compute graph consistency**: a request's dependency graph is evaluated relative to one graph version.

The guarantee is **not** historical filesystem reads. If a v10 snapshot first computes an uncached `PathMetadataKey(foo)` after the real file changed, it may read current disk state. That is acceptable for this milestone. A future daemon may choose to retry/taint requests if watcher events arrive during execution, but this milestone does not require that.

### 1.3 Why not removal-based invalidation

Removal-based invalidation cannot support DICE-style equality/backdating or concurrent snapshots well. If invalidation deletes old values/deps, an older snapshot has nothing to reuse, and a recomputation cannot compare against the previous value/deps to stop propagation.

DICE instead marks nodes dirty at a version, retains value/deps metadata, drains reverse dependencies, and later decides whether recomputation actually changed the value.

## 2. Concept mapping

| Concept | Buck2 DICE | Salsa | Pixi target |
|---|---|---|---|
| Request view | `DiceTransaction` at version | snapshot DB handle | `ComputeSnapshot` / transaction at graph version |
| Master mutation | `DiceTransactionUpdater::changed/changed_to` | `set` on master DB, blocks snapshots | master-only `changed` / `changed_to` / `invalidate_all` |
| Current version | `VersionNumber` | revision counter | global graph version |
| Active versions | `VersionTracker` with ref counts | snapshot clone count | `VersionTracker` with per-version ref counts |
| Per-version task cache | `SharedCache` per active version | per-thread/local runtime state | per-version in-flight/completed task cache |
| Normal computed key | `Key` / `StorageType::Normal` | tracked function | `Key` |
| Injected input | `StorageType::Injected` / `changed_to` | input field | existing `InjectedKey`, upgraded to versioned values |
| FS leaf | normal key changed by watcher | input or on-demand input | normal `Key`: `PathMetadataKey`, `ReadDirKey` |
| Dirty missing key | `VacantGraphNode` | input changed_at without memo | vacant dirty marker |
| Reverse deps | per-node `rdeps` | mostly forward deps + red/green checks | per-node rdeps or equivalent reverse index owned by graph |
| Change pruning | `Key::equality`, value/deps reusable | backdating `changed_at` | wire existing `Key::equality` into recompletion/backdating |
| Full reset | `unstable_take` / graph clear | new DB or synthetic write | `invalidate_all` / `unstable_drop_everything` |

## 3. Public API target

Names can be adjusted to fit Pixi style, but the semantics should be preserved.

```rust
impl ComputeEngine {
    /// Create a read-only snapshot at the latest committed graph version.
    pub fn snapshot(&self) -> ComputeSnapshot;

    /// Convenience API, if retained, computes in a fresh current snapshot.
    pub async fn compute<K: Key>(&self, key: &K) -> Result<K::Value, ComputeError>;

    /// Mark normal keys changed/force-dirty in a new graph version.
    pub fn changed<K: Key>(&self, keys: impl IntoIterator<Item = K>) -> GraphVersion;

    /// Compatibility alias for changed([key]).
    pub fn invalidate<K: Key>(&self, key: &K) -> GraphVersion;

    /// Set/inject values at a new graph version.
    pub fn changed_to<K: Key>(&self, values: impl IntoIterator<Item = (K, K::Value)>) -> GraphVersion;

    /// Compatibility API for existing InjectedKey callers.
    pub fn re_inject<K: InjectedKey>(&self, key: &K, value: K::Value) -> GraphVersion;

    /// Drop all graph state and reject/deactivate old versions according to implementation rules.
    pub fn invalidate_all(&self) -> GraphVersion;
}

impl ComputeSnapshot {
    pub fn version(&self) -> GraphVersion;
    pub async fn compute<K: Key>(&self, key: &K) -> Result<K::Value, ComputeError>;
}
```

Important API constraints:

- snapshots must not expose invalidation/reinjection;
- `changed` on a missing key must create a dirty/vacant node;
- `changed_to` with equal value should avoid rdep invalidation;
- old snapshot completions must be rejected if their version/epoch is no longer active.

## 4. Internal architecture target

### 4.1 Prefer a graph owner

Prefer a DICE-like graph owner task/thread that serializes graph state mutation:

- version allocation/commit;
- graph lookup;
- graph update after compute;
- invalidation/dirty propagation;
- active-version refcounting;
- full reset.

Workers may compute concurrently. They send graph lookup/update messages to the owner. This avoids lock-order bugs that would be easy to introduce with per-type locks plus a global reverse map.

### 4.2 Version tracker

Minimal shape:

```rust
struct VersionTracker {
    current: GraphVersion,
    invalid_before: GraphVersion,
    active_versions: HashMap<GraphVersion, ActiveVersionData>,
    epoch_counter: u64,
}

struct ActiveVersionData {
    ref_count: usize,
    epoch: VersionEpoch,
    task_cache: SharedTaskCache,
}
```

Behavior:

- `snapshot()` gets `current`, increments `active_versions[current].ref_count`, and returns `(version, epoch, task_cache)`.
- Dropping a snapshot decrements the ref count.
- When ref count reaches zero, drop/cancel that version's per-version task cache.
- Computation completions include `(version, epoch)` and are rejected if the version/epoch is no longer active.
- `invalidate_all` may bump `current`, set `invalid_before`, clear graph state, and reject older versions; exact behavior must be tested/documented.

### 4.3 Versioned graph nodes

Minimal DICE-inspired shape:

```rust
enum GraphNode<K: Key> {
    Occupied(OccupiedNode<K>),
    Injected(InjectedNode<K>),
    VacantDirty(VacantDirtyNode),
}

struct OccupiedNode<K: Key> {
    value: K::Value,
    deps: Deps,
    rdeps: Rdeps,
    verified_ranges: VersionRanges,      // or minimal equivalent
    force_dirty_history: ForceDirtyHistory,
}

struct InjectedNode<K: Key> {
    values: VersionedValues<K::Value>,    // injected values cannot be recomputed once dropped
    rdeps: Rdeps,
}

struct VacantDirtyNode {
    force_dirty_history: ForceDirtyHistory,
}
```

A first implementation may simplify `VersionRanges` if it still supports active snapshots and equality/backdating correctly. Do not delete old values as the primary invalidation mechanism.

### 4.4 Dirty propagation

For `changed(key)` at new version `v`:

1. If key exists, mark it force-dirty at `v`.
2. If key does not exist, create `VacantDirty` with force-dirty marker at `v`.
3. Drain that node's current `rdeps`.
4. BFS through rdeps, marking each dependent dirty at `v` and draining its rdeps.
5. Commit version iff some graph state changed.

Rdeps are stored for invalidation. Like DICE, they only need to cover the latest non-invalidated rdeps; once a node is dirtied, its rdeps are drained and later rebuilt when dependents recompute.

### 4.5 Lookup and recompute

For `snapshot.compute(key)` at version `v`:

1. Graph lookup returns one of:
   - match: value valid at `v`;
   - check-deps: old value exists but needs dependency validation;
   - compute: no usable value;
   - rejected: version/epoch invalid.
2. Per-version task cache deduplicates concurrent computations at the same version.
3. During compute, dependency reads record deps.
4. On completion, update graph via graph owner with `(version, epoch, value, deps)`.
5. If old occupied value exists and `Key::equality(old, new)` is true and deps are equivalent, mark unchanged/backdated and do not dirty dependents further.
6. If value or deps changed, store new value/deps and keep dependents dirty for current/newer versions.

### 4.6 Injected / changed-to values

`changed_to(key, value)` should be DICE-like:

- If key has a previous injected/current value and `Key::equality(old, value)` is true, do not dirty rdeps.
- If changed, record the new value at the new version, close the old value's valid range, and dirty rdeps.
- If key was missing, create an injected/occupied value valid from the new version.

Keep existing `inject()` as initial setup if needed, but daemon-oriented code should migrate toward `changed_to` / `re_inject` semantics.

## 5. Filesystem graph invalidation mapping

Filesystem leaves should be normal computed keys, not injected keys.

| Filesystem event | Dirty keys |
|---|---|
| file content/metadata modified | `PathMetadataKey(path)`; later `ReadFileKey(path)` if present |
| file created/deleted/renamed | `PathMetadataKey(path)` and `ReadDirKey(parent)` |
| directory created/deleted/renamed | `ReadDirKey(parent)` and relevant `PathMetadataKey(dir)` |
| watcher overflow / missed events | `invalidate_all` or fs-key-type scoped reset if added |

Buck2 evidence: `FileChangeTracker` records sets of `ReadFileKey`, `ReadDirKey`, `PathMetadataKey`, and exact-case keys, then calls `ctx.changed(...)` for each set.

## 6. Equality / early cutoff

`Key::equality` must be wired from the beginning of the DICE-style design.

Rules:

- Returning false for equal values is safe but less incremental.
- Returning true for unequal values is unsound.
- Equality-based reuse requires both semantic value equality and dependency equivalence.
- For filesystem keys, equality should compare semantic filesystem state, not pointer identity.

Examples:

- `PathMetadataKey`: equal if `FileState` is equal.
- `ReadDirKey`: equal if sorted entry list is equal.
- `GlobSnapshotKey`: equal if matched file list and captured metadata snapshot are equal.

## 7. Concurrency and in-flight behavior

- Concurrent snapshots at the same graph version should share per-version in-flight computations.
- Snapshots at different versions must not share in-flight computations unless the implementation can prove it is safe.
- A computation finishing for an inactive `(version, epoch)` must be rejected/dropped.
- Dropping the last snapshot for a version should cancel pending tasks for that version where possible.
- Filesystem watcher commits are serialized by the graph owner and create newer versions. They must not mutate existing snapshot handles.

## 8. Correctness fallback

`invalidate_all` / `unstable_drop_everything` is required for watcher overflow, workspace reset, or uncertain state.

Recommended behavior:

- bump current version;
- mark older versions rejected or set `invalid_before`;
- clear graph nodes;
- drop/cancel per-version task caches as their snapshots drain;
- preserve unrelated global data (`DataStore`) unless explicitly reset.

## 9. Implementation order

1. Restore source tree safely.
2. Add `GraphVersion`, `VersionEpoch`, `VersionTracker`.
3. Add `ComputeSnapshot` and active-version lifecycle.
4. Add per-version task cache or adapt existing in-flight cache into per-version storage.
5. Introduce graph owner or equivalent serialized graph mutation boundary.
6. Change node storage to occupied/injected/vacant-dirty with rdeps and valid version metadata.
7. Implement `changed` / `invalidate`, dirty propagation, and missing-key dirty markers.
8. Implement `changed_to` / `re_inject`.
9. Wire equality/backdating in recompletion.
10. Add tests for concurrency, dirty propagation, vacant dirty nodes, equality, injected changes, active-version cleanup, and reset.
11. Only then build `pixi_compute_fs` on top.

## 10. Stop conditions

Stop and split the work if:

- graph-owner integration conflicts with current executor assumptions;
- `AnyKey` type erasure cannot support versioned graph state safely;
- active snapshot + invalidation semantics cannot be tested without a larger public API redesign;
- source restoration is unsafe.

## Sources consulted

- Buck2 DICE `VersionedGraph` docs and invalidation implementation: `dice/dice/src/impls/core/graph/storage.rs` at `d260d642f615dde7363dddff28bbecac406b8cab`.
- Buck2 DICE node state, `OccupiedGraphNode`, `InjectedGraphNode`, rdeps and equality paths: `dice/dice/src/impls/core/graph/nodes.rs` at same commit.
- Buck2 DICE `VersionTracker`: `dice/dice/src/impls/core/versions.rs` at same commit.
- Buck2 filesystem dirty mapping: `app/buck2_common/src/file_ops/dice.rs` at same commit.
- Salsa snapshot/concurrency docs: `book/src/plumbing/database_and_runtime.md` at `86e69d7eacdbc4ee87e183a147af02ee5d932ca1`.
- Salsa backdating docs/source: `book/src/reference/algorithm.md`, `src/function/backdate.rs` at same commit.
- Bazel filesystem/Skyframe research in `daemon-research/bazel-filesystem-monitoring.md`.

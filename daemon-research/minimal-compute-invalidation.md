# Minimal Compute-Engine Invalidation for a Pixi Daemon Prototype

Status: historical scouting report against commit `3b14a4892edc229275217ce1d168edf6e11836bd`.

> Superseded direction: the removal-based invalidation prototype described in parts of this document is no longer the target architecture. After plan grilling, the implementation target is DICE-style graph-version snapshots, dirty/vacant nodes, active-version refcounts, and equality/backdating. See `daemon-research/invalidation-research-mini-design.md` and `design/pixi-fs-graph-agent-handoff.md` for the current plan.

## Files Inspected

| File | Relevance |
|------|-----------|
| `docs/crates/pixi_compute_engine/src/key_graph.rs` (lines 40--350) | Core in-memory store: `GraphNode` states, `PerTypeSlot`, `KeyGraph`, `insert_completed`/`insert_injected` paths. The primary data structure to extend. |
| `docs/crates/pixi_compute_engine/src/engine.rs` (lines 85--179) | `ComputeEngine` handle, `inject` / `read` APIs, and injecion-single-write panic message citing "no invalidation mechanism". |
| `docs/crates/pixi_compute_engine/src/ctx.rs` (lines 80--130, 250--380) | `ComputeCtx` dep recording: `deps.lock().push(any_key)` on every `ctx.compute()`. On completion, `spawn_compute_future` calls `insert_completed(…, deps, …)`. |
| `docs/crates/pixi_compute_engine/src/key.rs` (lines 107--130) | `Key::equality` exists and is documented "for future early cutoff: when a dependency recomputes to the same value, dependents do not need to recompute". Default returns `false`. |
| `docs/crates/pixi_compute_engine/src/injected.rs` (lines 18--43) | "Single-write contract… Overwriting is forbidden because the engine has no invalidation mechanism". |
| `docs/crates/pixi_compute_engine/src/any_key.rs` (full) | `AnyKey`: type-erased handle used in dep lists. Implements `Hash + Eq + Display`, cheap `Arc` clone. |
| `docs/crates/pixi_command_dispatcher/src/command_dispatcher/builder.rs` (lines 346--548) | `finish()`: builds engine, injects `CacheDirsKey`, `EnvVarsKey`, `ChannelConfigKey`, `EnabledProtocolsKey`, `ToolBuildEnvironmentKey`, `BackendOverrideKey`. All are single-write `InjectedKey`s. |
| `docs/crates/pixi_command_dispatcher/src/command_dispatcher/mod.rs` (lines 51--174, 300--310) | `CommandDispatcherData` holds gateway, caches, resolvers; `clear_filesystem_caches()` only clears `GlobHashCache`, not compute-engine graph entries. |
| `docs/crates/pixi_command_dispatcher/src/injected_config.rs` (full) | Concrete injected keys used by pixi. |
| `docs/crates/pixi_core/src/workspace/mod.rs` (lines 783--848) | `command_dispatcher_builder()`: per-workspace dispatcher construction, the seam where a daemon would keep a long-lived dispatcher. |
| `design/pixi-daemon.md` (full) | Existing daemon motivation and role sketch; explicitly names the need to "invalidate state" in the compute engine. |
| `daemon-research/design-local-seams.md` (full) | Prior analysis of existing seams: notes injected-key immutability is a blocker, suggests coarse generation-based invalidation as fallback. |

## Key Architecture Summary

### The compute engine's in-memory state

```
KeyGraph
  inner: Mutex<HashMap<TypeId, Arc<dyn TypedSlot>>>
    └─ PerTypeSlot<K>
         nodes: HashMap<K, GraphNode<K>>
           ├─ InFlight { future: WeakShared<…>, generation: u64 }
           ├─ Completed { value: K::Value, deps: Vec<AnyKey> }    ← target slice
           └─ Injected  { value: K::Value }
```

- **Forward dependencies** are recorded as `deps: Vec<AnyKey>` inside each `Completed` node, built incrementally by `ctx.compute(&child)` calls inside a parent's compute body.
- **No reverse index** exists. There is no way to answer "which nodes depend on key X?" without scanning every per-type slot.
- **Values are immutable for the engine's lifetime.** There is no `invalidate()`, no `remove()`, no `re_inject()`.
- **Injected keys** are write-once (panic on double-inject).
- **`Key::equality`** is already in the trait for **early cutoff** (re-compute but skip downstream invalidation if value unchanged), but it is not wired to any invalidation path because invalidation does not exist.

### How pixi uses the compute engine today

Each CLI invocation creates a **fresh** `CommandDispatcher` → fresh `ComputeEngine` → fresh `KeyGraph`. No state is shared across invocations. The builder injects six `InjectedKey` types at construction:

| Injected Key | Value Type | Sensitivity |
|---|---|---|
| `CacheDirsKey` | `Arc<CacheDirs>` | Changes when workspace root/pixi config changes |
| `EnvVarsKey` | `Arc<HashMap<String,String>>` | Every invocation, snapshots `std::env::vars()` |
| `ChannelConfigKey` | `Arc<ChannelConfig>` | Changes with pixi.toml channels |
| `EnabledProtocolsKey` | `Arc<EnabledProtocols>` | Changes with build-protocol discovery config |
| `ToolBuildEnvironmentKey` | `Arc<BuildEnvironment>` | Changes with tool platform config |
| `BackendOverrideKey` | `Arc<BackendOverride>` | Changes with backend override config |

After injection, Keys like `SolveCondaEnvironmentKey`, `SourceMetadataKey`, `InputGlobSetWalkKey`, `BuildBackendMetadataKey`, `InstantiateBackendKey` etc. run through the engine. Their dependency edges are recorded in `Completed.deps`.

## What Can Be Invalidated Initially

### State slice to target: `Completed` entries in `PerTypeSlot.nodes`

These are the **cached computed results** for every key the engine has ever run. For a daemon, the goal is to keep as many as possible warm while correctly invalidating the ones whose inputs changed.

### Three invalidation triggers (priority order for `pixi run`)

#### 1. Injected-key re-write (highest frequency)

Every `pixi run` may have different env vars, different config flags (`--platform`, `--environment`). Today these are snapshotted once at dispatcher construction; for a daemon they must be re-snapshot per-request or re-injected on change.

**Initial scope**: Allow `ComputeEngine::re_inject(key, new_value)` that:
- Updates the `Injected` node's value
- Recursively invalidates all transitive dependents (via the reverse index)
- Re-runs on next `engine.compute()`

**For `pixi run`**: The `EnvVarsKey` snapshot is the primary churn point. The daemon would either:
- (A) Re-inject `EnvVarsKey` with a fresh snapshot before each run request, or
- (B) Accept per-request env overrides as a new compute-key parameter, keeping `EnvVarsKey` as daemon-start env only.

Option A is simplest for a prototype.

#### 2. Filesystem-watch driven invalidation (medium frequency)

When source files, `pixi.toml`, `pyproject.toml`, `pixi.lock`, or config files change on disk, the corresponding keys' cached results are stale.

**Initial scope**: Provide `ComputeEngine::invalidate(&key)` that:
- Looks up the key in its `PerTypeSlot`
- Removes it from the `Completed` or `InFlight` maps
- Recursively invalidates all dependents found via the reverse index
- Uses `Key::equality` to optionally stop propagation (early cutoff)

**For `pixi run`**: The daemon would watch files and map change events to key invalidation. For example:
- `pixi.toml` / `pyproject.toml` change → invalidate all keys in the manifest-derivation chain
- `pixi.lock` change → invalidate lock-dependent keys (satisfiability, solve results)
- Source file change → invalidate `InputGlobSetWalkKey` and its dependents (source metadata, backend metadata, build results)
- `.pixi/envs/` prefix change → invalidate prefix-validation keys

#### 3. Coarse engine reset (low frequency, fallback)

When too many things change (e.g., workspace relocation, major config restructuring), wholesale engine replacement is simpler than fine-grained invalidation.

**Initial scope**: Keep the existing `ComputeEngine::new()` pattern available. The daemon can drop the old `EngineInner` `Arc` and build a fresh engine when coarse changes are detected.

### What NOT to invalidate initially

- **`InFlight` entries**: Canceling in-flight computation is complex (requires task abort coordination). For a prototype, let in-flight computations finish and use their results; if they are stale, invalidate after completion.
- **Active-edge graph**: Cycle-detection edges are ephemeral (created and dropped within a single `ctx.compute()` call). They don't need invalidation.
- **`DataStore` global data**: Resources like `Gateway`, `PackageCache`, resolvers do not need invalidation — they are shared dependencies, not computed values. Their internal state (connection pools, caches) is managed separately.

## Required API Shape

### Additions to `ComputeEngine` (public)

```rust
impl ComputeEngine {
    /// Invalidate a specific key and all keys that transitively depend on it.
    /// Returns the number of nodes removed.
    ///
    /// If the key is in-flight, the in-flight entry is removed but the
    /// running task is not aborted (it will complete in the background
    /// and its result will be discarded because the generation no longer
    /// matches).
    pub fn invalidate<K: Key>(&self, key: &K) -> usize;

    /// Re-inject a value for an InjectedKey, replacing any previously
    /// injected value. All keys that depend on this injected key are
    /// transitively invalidated.
    ///
    /// This replaces the original single-write contract with a
    /// "re-inject invalidates dependents" contract.
    pub fn re_inject<K: InjectedKey>(&self, key: &K, value: K::Value);

    /// Check whether a key has a completed (or injected) entry in the cache.
    pub fn is_cached<K: Key>(&self, key: &K) -> bool;

    /// Invalidate all completed and injected entries, resetting the
    /// engine to a fresh state. In-flight computations are not aborted.
    /// Global data is preserved.
    pub fn invalidate_all(&self);
}
```

### Additions to `KeyGraph` (private)

```rust
impl KeyGraph {
    /// Remove a key from its per-type slot and return the set of
    /// dependents that must also be invalidated. Uses the reverse index.
    fn remove_and_collect_dependents<K: Key>(&self, key: &K) -> Vec<AnyKey>;

    /// Build or update the reverse dependency index.
    /// Called on insert_completed and removed on invalidation.
    fn add_reverse_edges(&self, parent: &AnyKey, deps: &[AnyKey]);
    fn remove_reverse_edges(&self, parent: &AnyKey, deps: &[AnyKey]);
}
```

### Reverse dependency index (new structure in `KeyGraph`)

```rust
pub(crate) struct KeyGraph {
    inner: Mutex<HashMap<TypeId, Arc<dyn TypedSlot>>>,
    // NEW: reverse index: key → set of keys that depend on it
    reverse: Mutex<HashMap<AnyKey, HashSet<AnyKey>>>,
}
```

The reverse index is populated when `insert_completed` writes `deps`, and cleaned up when a node is invalidated. It lives behind its own `Mutex` to avoid coupling with the per-type slot locks.

### Changes to `insert_completed`

Current signature:
```rust
pub(crate) fn insert_completed<K: Key>(&self, key: &K, value: K::Value, deps: Vec<AnyKey>, generation: SpawnGeneration)
```

After: additionally register `(parent_any_key, dep_any_key)` pairs in the reverse index.

### Invalidation algorithm

```
invalidate(target):
  queue = [target]
  removed = 0
  while queue not empty:
    key = queue.pop()
    slot = per_type_slot_for(key)    // via TypeId from AnyKey
    node = slot.nodes.remove(&key)   // remove from whichever state
    if node is Completed or Injected:
      removed += 1
      for depender in reverse_index[key]:
        queue.push(depender)
      remove reverse_index entries for key
  return removed
```

**Early cutoff** (optional, post-prototype): before pushing a depender, re-compute the invalidated key and compare with `Key::equality(old_value, new_value)`. If equal, skip invalidation of that depender.

### For `re_inject`

```
re_inject(key, new_value):
  invalidate(key)  // removes old Injected node + dependents
  insert_injected(key, new_value)  // fresh injection (will not panic because old was removed)
```

This approach avoids adding a special "replace injected" path in `KeyGraph`; it reuses invalidation + insert.

## State Slice to Target for Repeated `pixi run`

The daemon's **session object** is: one `CommandDispatcher` per workspace root, kept alive across invocations. The `CommandDispatcher` owns the `ComputeEngine`. The target state slices that need invalidation between `pixi run` calls:

### Slice A: Injected snapshot keys

| Key | What changes between runs | Strategy |
|-----|--------------------------|----------|
| `EnvVarsKey` | Environment variables (especially `PIXI_PROJECT_MANIFEST`, `PIXI_IN_SHELL`, `CONDA_OVERRIDE_*`) | `re_inject` before each run with a fresh `std::env::vars()` snapshot |
| `ChannelConfigKey` | Rare; when pixi.toml channels change | `re_inject` on manifest change |
| `BackendOverrideKey` | Rare; on config change | `re_inject` on manifest change |
| `CacheDirsKey` | Only when workspace root changes | Coarse engine reset |

### Slice B: Filesystem-dependent computed keys

These are the expensive, repeated computations that a daemon should cache:

| Key Family | Depends on | Invalidation trigger |
|-----------|-----------|---------------------|
| `InputGlobSetWalkKey` | Files matching glob patterns | File create/modify/delete within source tree |
| `SourceMetadataKey` | Source checkout + backend metadata | Source file change, `pixi.toml` source spec change |
| `BuildBackendMetadataKey` | Backend binary, source config | Backend binary change, source manifest change |
| `DiscoveredBackendKey` | Directory contents, enabled protocols | Directory change, config change |
| `SolveCondaEnvironmentKey` | Repodata, channel list, spec set | Repodata gateway cache change (internal), spec change |
| `InstallPixiEnvironmentSpec` keys | Prefix contents | Prefix modification outside pixi |

For `pixi run`, **minimum viable invalidation** starts with just injected-key re-injection and a single coarse `invalidate_all()` before each request. Fine-grained filesystem invalidation can be layered in after the prototype works.

### Slice C: External caches (not in compute graph)

These are stored in `CommandDispatcherData` and also need invalidation:

| Cache | Location | Clearing mechanism |
|-------|----------|-------------------|
| `GlobHashCache` | `CommandDispatcherData.glob_hash_cache` | Already has `.clear()` — call on filesystem change |
| `BuildBackendMetadataCache` | On-disk, under cache dirs | Already has invalidation via cache revision |
| `PackageCache` | rattler's on-disk package cache | Managed by rattler |

## Minimal Prototype Plan

### Phase 0: Reverse index + invalidate (engine-only, no pixi wiring)

**Files to change:**
- `docs/crates/pixi_compute_engine/src/key_graph.rs`: add `reverse: Mutex<HashMap<AnyKey, HashSet<AnyKey>>>` to `KeyGraph`, populate on `insert_completed`, add `remove_and_collect_dependents()` private method.
- `docs/crates/pixi_compute_engine/src/engine.rs`: add `invalidate()`, `re_inject()`, `is_cached()`, `invalidate_all()` public methods.

**Tests:** Add to `docs/crates/pixi_compute_engine/tests/integration/` — a new `invalidation.rs` test file exercising:
- Invalidate a leaf key (no dependents).
- Invalidate a key with dependents (transitive cascade).
- Re-inject: dependents invalidated.
- `invalidate_all` clears everything.
- Early cutoff with `Key::equality` returning `true`.

### Phase 1: Wire into CommandDispatcher for a daemon session

**Files to change:**
- `docs/crates/pixi_command_dispatcher/src/command_dispatcher/mod.rs` or a new `daemon.rs`: Add `re_snapshot_env()` that calls `engine.re_inject(&EnvVarsKey, fresh_snapshot)`.
- Call `re_snapshot_env()` + `clear_filesystem_caches()` between daemon requests.

### Phase 2: Filesystem watching → fine-grained invalidation

Add a watcher (`notify` crate) in the daemon process that maps filesystem events to `engine.invalidate(&SpecificKey)` calls. This requires key-type-specific mapping (e.g., which glob patterns belong to which `InputGlobSetWalkKey` instances), which is a design task for Phase 2.

## Risks and Open Questions

1. **`AnyKey` as reverse-index key**: `AnyKey` wraps an `Arc<dyn AnyKeyDyn>` and implements `Hash + Eq` correctly (folding `TypeId`). This is sufficient for reverse-index use, but the `reverse` map lookup needs to downcast back to the concrete key type to remove from `PerTypeSlot`. Currently `AnyKey` cannot do that — the reverse index must store enough info to locate the per-type slot. **Solution**: store `(TypeId, Box<dyn AnyKeyDyn>)` or use the existing `AnyKey` with a helper that resolves the slot via `TypeId` and removes by key identity. The `PerTypeSlot` removal needs the concrete `K`, so we need a method on `TypedSlot` like `fn remove_any(&self, key: &AnyKey) -> bool` that downcasts and delegates to `HashMap::remove`.

2. **In-flight invalidation**: If a key is `InFlight` when `invalidate()` is called, we cannot simply remove it — the spawned task will still call `insert_completed` and may re-populate `Completed` after invalidation. **Initial strategy**: just remove the `InFlight` entry (so new callers will re-spawn), and guard `insert_completed` with `SpawnGeneration` (which it already does). If the generation no longer matches, the stale task's result is silently dropped. This already works because `insert_completed` checks `generation`.

3. **Concurrent invalidation + compute**: If `invalidate()` removes a `Completed` node while another caller is reading it via `Lookup::Completed(value)`, the reader has already cloned the value. This is safe (the value is `Clone`, the reader proceeds with the old value, the next reader will re-compute). The reverse index updates under `reverse` lock, while per-type slot operations are under per-type locks — these are independent, avoiding deadlock.

4. **`invalidate_all` vs rebuilding**: For a prototype, `invalidate_all()` is sufficient. It's better than creating a new engine because it preserves `DataStore` global data (gateway, caches, resolvers), which are expensive to rebuild. The daemon should prefer `invalidate_all()` over creating a new `CommandDispatcher` when the workspace root hasn't changed.

5. **Daemon CLI interaction**: The daemon should expose `invalidate`, `re_inject`, and `invalidate_all` through its RPC interface so the CLI can request cache invalidation explicitly (e.g., `pixi daemon invalidate --key EnvVarsKey`). Filesystem watching should be the primary trigger, but explicit invalidation is a useful escape hatch.

## Start Here

Begin implementation in `docs/crates/pixi_compute_engine/src/key_graph.rs` by:

1. Adding the `reverse: Mutex<HashMap<AnyKey, HashSet<AnyKey>>>` field to `KeyGraph`.
2. Populating it in `insert_completed` after the generation check passes.
3. Adding `TypedSlot::remove_any(&self, key: &AnyKey) -> bool` to the trait and implementing it on `Mutex<PerTypeSlot<K>>`.
4. Adding `KeyGraph::remove_and_collect_dependents` that uses `remove_any` and returns the set of transitive dependents from the reverse index.
5. Then adding `ComputeEngine::invalidate` and `ComputeEngine::re_inject` in `engine.rs`.

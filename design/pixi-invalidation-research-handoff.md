# Pixi compute invalidation research handoff

## Purpose

Before implementing invalidation in `pixi_compute_engine`, run a focused research/design pass on established incremental systems. The goal is to make Pixi's first invalidation API compatible with proven concepts: changed inputs, snapshots/transactions, reverse dependencies, equality/change pruning, injected values, in-flight computation behavior, and concurrent filesystem updates.

This is a **required pre-implementation gate** for the combined `pixi_compute_engine` + `pixi_compute_fs` milestone.

## Updated decision

The research phase concluded that Pixi should stay close to **Buck2 DICE** for this milestone.

The previous hypothesis — reverse dependency index plus removal-based invalidation — is superseded. Pixi's daemon needs concurrent requests and concurrent filesystem monitoring. Therefore invalidation must be versioned/dirty-state based rather than deleting cached nodes.

Primary design output:

- `daemon-research/invalidation-research-mini-design.md`

## Required semantics

- A request gets a read-only compute snapshot/transaction at a graph version.
- Filesystem watcher events may be committed concurrently to newer graph versions.
- Existing snapshots are not invalidated or mutated.
- New snapshots see the latest committed version.
- Snapshots do not expose invalidation/reinjection APIs.
- We require graph-version consistency, not true historical OS filesystem snapshots.

Example:

```text
request A starts at v10
watcher commits PathMetadataKey(foo) dirty at v11
request B starts at v11
request A continues at v10
```

## Systems studied

1. **Buck2 / DICE** — primary model
   - `VersionTracker` and active versions.
   - Per-version transaction/task cache.
   - `VersionedGraph` with occupied/injected/vacant nodes.
   - Dirty propagation through rdeps.
   - `changed` / `changed_to` APIs.
   - `Key::equality` and value/deps reuse.
   - Filesystem changes translated to changed DICE keys.

2. **Salsa** — useful contrast
   - Revisions and backdating.
   - Snapshot handles are read-only, but writes block/cancel/wait for snapshots rather than commit concurrent graph versions.
   - This is not sufficient for Pixi's desired concurrent watcher + concurrent request model unless we queue writes.

3. **Bazel / Skyframe** — filesystem leaf modeling and fallback
   - `FileStateValue` / `DirectoryListingStateValue` as filesystem leaf nodes.
   - `DiffAwareness` and fallback scanning.
   - Dirty/inject flow.
   - Change pruning when recomputation produces the same value.

## Local context to read first

- `design/pixi-daemon.md`
- `design/pixi-fs-graph-implementation-handoff.md`
- `daemon-research/invalidation-research-mini-design.md`
- `daemon-research/minimal-compute-invalidation.md` — historical context; removal-based plan is superseded.
- `daemon-research/buck2-filesystem-monitoring.md`
- `daemon-research/bazel-filesystem-monitoring.md`

Relevant Pixi source areas after source restoration:

- `crates/pixi_compute_engine/src/key_graph.rs`
- `crates/pixi_compute_engine/src/engine.rs`
- `crates/pixi_compute_engine/src/ctx.rs`
- `crates/pixi_compute_engine/src/key.rs`
- `crates/pixi_compute_engine/src/injected.rs`
- `crates/pixi_compute_engine/src/any_key.rs`

## Required implementation design points

The mini-design must be followed or explicitly revised with rationale:

1. **Snapshot/transaction API**
   - Public read-only snapshot handle.
   - Master-only changed/changed-to/reset APIs.

2. **Version tracker**
   - Global monotonic graph version.
   - Active-version ref counts.
   - Per-version task cache cleanup when refcount reaches zero.
   - Version/epoch rejection for stale completions.

3. **Graph owner / serialization**
   - Prefer DICE-like single graph-owner task/thread.
   - Worker computations can run concurrently; graph mutation is serialized.

4. **Versioned graph nodes**
   - Occupied nodes retain value/deps metadata.
   - Injected nodes retain versioned values or equivalent safe data.
   - Missing invalidated keys become vacant dirty markers.

5. **Reverse dependencies**
   - Store rdeps for invalidation.
   - Drain rdeps when a node is marked dirty.
   - Rebuild rdeps when dependents recompute.

6. **Changed input / reinjection strategy**
   - Filesystem leaves are normal computed keys.
   - Configuration/env-like values use changed-to/reinject semantics.

7. **Equality / early cutoff**
   - Existing `Key::equality` must be wired as DICE-style change pruning/backdating.
   - Equality reuse requires value equality and dependency equivalence.

8. **In-flight computation behavior**
   - Deduplicate work within a version.
   - Reject completions for inactive/outdated version+epoch.
   - Cancel pending per-version tasks when the last snapshot for that version drops where possible.

9. **Correctness fallback**
   - Full graph reset for watcher overflow or uncertain state.
   - Reset behavior with active snapshots must be documented and tested.

## Non-goals

- Do not design the daemon IPC API here.
- Do not move lockfile or `pixi run` ownership into the daemon.
- Do not introduce true historical filesystem snapshots.
- Do not implement the filesystem watcher in this milestone.
- Do not implement broad semantic invalidation for every Pixi state slice before the compute substrate is proven.

# Agent handoff: DICE-style compute snapshots + `pixi_compute_fs`

## Mission

Implement the next milestone in the order below:

1. Restore/verify the Pixi source worktree safely.
2. Implement a DICE-style `pixi_compute_engine` foundation for concurrent daemon requests and concurrent filesystem invalidation.
3. Add the `pixi_compute_fs` crate only after the compute-engine substrate is in place.
4. Add Criterion benchmarks for glob snapshot/freshness performance.

The previous "minimal invalidation = remove cached nodes" plan is superseded. We want to stay close to Buck2 DICE because the daemon must support concurrent requests while a filesystem watcher keeps committing changes.

Do **not** implement the daemon yet. Do **not** decide daemon IPC/API yet. Do **not** implement a filesystem watcher yet.

## New architectural decision

Use **graph-version consistency**, not true historical filesystem snapshots.

A request should run against a compute snapshot/transaction at a specific graph version. Filesystem watcher events may be committed concurrently to newer graph versions. Existing snapshots must not be mutated or invalidated. New snapshots see the latest committed version.

```text
request A starts at v10
watcher sees file change and commits dirty FS keys at v11
request B starts at v11
request A continues at v10
```

This does **not** require historical OS filesystem semantics. If request A first reads an uncached file after the physical file changed, it may observe current disk state. That is acceptable for this milestone. The guarantee is internal compute-graph consistency, not time-travel filesystem reads.

## Worktree warning

The active worktree currently appears to have many `crates/...` files deleted/missing, while a source copy exists in git and under `docs/crates/`.

Before implementing source changes:

1. Inspect `git status --short`.
2. Confirm whether `crates/` is intentionally missing or accidentally deleted.
3. If restoration is safe, restore/check out the intended source state before editing source.
4. Preserve all design/research artifacts under `design/` and `daemon-research/`.
5. Stop and ask before any destructive restore if user changes may be at risk.

## Required reading before editing source

- `daemon-research/invalidation-research-mini-design.md` — now the primary DICE-style compute design.
- `design/pixi-fs-graph-implementation-handoff.md`
- `design/pixi-invalidation-research-handoff.md`
- `daemon-research/minimal-compute-invalidation.md` — historical scouting; treat removal-based invalidation as superseded.
- `daemon-research/minimal-state-slice.md`
- `daemon-research/buck2-filesystem-monitoring.md`
- `daemon-research/bazel-filesystem-monitoring.md`

Relevant source areas once restored:

- `crates/pixi_compute_engine/src/key_graph.rs`
- `crates/pixi_compute_engine/src/engine.rs`
- `crates/pixi_compute_engine/src/ctx.rs`
- `crates/pixi_compute_engine/src/key.rs`
- `crates/pixi_compute_engine/src/injected.rs`
- `crates/pixi_compute_engine/src/any_key.rs`
- `crates/pixi_glob/src/**`
- `crates/pixi_command_dispatcher/src/keys/input_glob_set_walk.rs`
- `crates/pixi_command_dispatcher/src/input_globs.rs`

## Architectural contract

All filesystem inputs for glob snapshot/freshness must flow through compute-engine keys.

Do **not** build a custom side cache where a future watcher directly mutates glob results. The future watcher should only mark low-level filesystem keys changed/dirty (`PathMetadataKey`, `ReadDirKey`, later `ReadFileKey` if needed). Derived keys (`GlobSnapshotKey`, `GlobFreshnessKey`) must become stale through compute-engine dependencies.

## Scope

### In scope, phase 1: DICE-style `pixi_compute_engine`

Replace the previous removal-based invalidation target with a DICE-inspired versioned graph foundation.

Required public concepts/API shape:

- `ComputeEngine::snapshot()` or equivalent transaction creation API.
- A read-only compute handle for snapshots/transactions, e.g. `ComputeSnapshot` / `ComputeTransaction`.
- Master-only update APIs:
  - `changed(&key)` / `invalidate(&key)` for normal keys.
  - `changed_to(&key, value)` or `re_inject` for injected/replaced values.
  - `invalidate_all()` / `unstable_drop_everything()` fallback.
- Snapshot handles must not expose invalidation/reinjection.
- Existing `ComputeEngine::compute(&key)` may remain as a convenience over a current snapshot if that is less disruptive, but daemon-facing design should use explicit snapshots.

Required internal concepts:

- A global monotonic graph version/generation.
- Active-version tracking with ref counts.
- Per-version in-flight task cache so concurrent requests at the same version share work.
- Versioned graph nodes that retain old value/deps metadata instead of deleting on invalidation.
- Reverse dependencies (`rdeps`) attached to dependency nodes and drained on invalidation.
- Dirty/vacant nodes: invalidating a missing key must still record a dirty marker, like DICE `VacantGraphNode`.
- Direct invalidations of normal keys are force-dirty at the new version.
- Rdeps are marked dirty transitively.
- Equality/backdating via `Key::equality` is part of the model, not a later bolt-on.
- Injected values should keep versioned values or an equivalent safe strategy because they cannot be recomputed after being dropped.
- Old per-version task state must be cleaned when no snapshots reference that version.

Recommended internal architecture:

- Prefer a DICE-like single graph-owner task/thread that serializes graph lookup/update/invalidation/version bookkeeping.
- Worker tasks may compute concurrently, but graph state mutations should flow through the owner to avoid lock-order complexity.
- If a graph owner is too large for the first slice, stop and report the smallest safe staged alternative.

### In scope, phase 2: `pixi_compute_fs`

Add a reusable crate for filesystem-as-compute-graph primitives after the compute engine supports DICE-style snapshots/invalidation well enough to support it.

Suggested public concepts:

- `FileState`
  - file type/kind
  - size
  - mtime
  - inode/ctime where available and practical
  - shape should allow future digest-based mode
- `PathMetadataKey(path)`
- `ReadDirKey(dir)`
- `GlobSpec { root, patterns, markers, exclude_hidden }`
- `GlobSnapshotKey(GlobSpec)`
- `GlobSnapshot`
  - matched files
  - deterministic ordering
  - metadata/freshness data needed for comparison
- `GlobFreshnessKey { spec, previous_snapshot }`
- `GlobFreshness::{Fresh, Stale { reason }}`

`GlobSnapshotKey` is the primitive. `GlobFreshnessKey` is derived.

### In scope, phase 3: Benchmarks

Add Criterion benchmarks from day one for `pixi_compute_fs` if that crate is reached in this session.

Compare against:

- current `pixi_glob`
- `globwalk`
- raw `ignore` walker if practical
- cold `pixi_compute_fs::GlobSnapshotKey`
- warm repeated `GlobSnapshotKey` / `GlobFreshnessKey`
- invalidation + recompute for small changes

Benchmark scenarios should include at least:

- tiny tree
- medium tree
- huge flat directory
- huge nested tree
- many patterns
- many exclusions
- hidden files excluded/included
- marker-based pruning
- source-package-like tree with `pyproject.toml`, `src/**`, generated dirs
- no changes between repeated runs
- one file mtime changed
- one file added matching glob
- one file added not matching glob

### Out of scope

- No daemon process.
- No filesystem watcher implementation.
- No IPC/API decisions.
- No `pixi run` integration.
- No source metadata integration yet.
- No lockfile/prefix/activation movement into daemon.
- No broad semantic invalidation for all Pixi state beyond the compute-engine foundation needed by fs graph.
- No true historical filesystem snapshot semantics.

## Suggested implementation plan

### Phase 0: Restore/read/verify baseline

1. Ensure source files are present and not accidentally deleted.
2. Run focused baseline tests if feasible:

```bash
cargo test -p pixi_compute_engine
cargo test -p pixi_glob
```

If tests cannot run due environment or workspace state, record why.

### Phase 1: DICE-style compute-engine foundation

Target files likely include:

- `crates/pixi_compute_engine/src/key_graph.rs`
- `crates/pixi_compute_engine/src/engine.rs`
- `crates/pixi_compute_engine/src/ctx.rs`
- `crates/pixi_compute_engine/src/any_key.rs`
- `crates/pixi_compute_engine/src/injected.rs`
- new modules for versions / transaction cache / graph owner if useful

Implementation targets:

1. Introduce graph versions and active-version refcounting.
2. Introduce snapshot/transaction handles.
3. Add serialized graph mutation path, preferably graph-owner task/thread.
4. Convert node state from `Completed`/`Injected` removal semantics to versioned occupied/injected/vacant-dirty semantics.
5. Track rdeps per node and drain rdeps on invalidation.
6. Implement dirty propagation to transitive rdeps.
7. Implement equality/backdating behavior:
   - recompute dirty node;
   - if new value equals old value and deps are equivalent, mark unchanged/valid at current version;
   - otherwise update value/deps and keep dependents dirty.
8. Implement changed-to/reinject behavior for injected inputs.
9. Ensure stale in-flight completions cannot update graph state after their version/epoch is inactive.
10. Add tests.

Expected test cases:

- snapshot sees stable graph version while newer invalidations are committed;
- concurrent snapshots at same version share in-flight work;
- active-version refcount cleanup drops per-version task state;
- invalidating a cached leaf marks it dirty and dirties direct dependents;
- transitive dirty propagation works;
- invalidating a missing key creates a vacant dirty marker;
- unrelated keys remain clean/cached;
- direct force-dirty normal key recomputes even when deps are unchanged;
- equality/backdating avoids dependent recomputation when value and deps are unchanged;
- changed-to/reinject with equal value is a no-op for dependents;
- changed-to/reinject with changed value dirties dependents;
- invalidate-all rejects/drops old versions safely or clearly documents behavior.

Important safety points:

- Avoid lock-order deadlocks; prefer single graph owner.
- Do not expose invalidation APIs on snapshots.
- Do not mutate active snapshot versions when filesystem changes are committed.
- Do not assume historical disk reads are possible.
- Be explicit about cancellation/outdated completion behavior.

### Phase 2: Add `pixi_compute_fs` crate

Add `crates/pixi_compute_fs` to the workspace.

Suggested module layout:

```text
crates/pixi_compute_fs/
  Cargo.toml
  src/
    lib.rs
    file_state.rs
    glob_spec.rs
    snapshot.rs
    keys/
      mod.rs
      path_metadata.rs
      read_dir.rs
      glob_snapshot.rs
      glob_freshness.rs
  benches/
    glob_snapshot.rs
```

Suggested dependencies:

- `pixi_compute_engine`
- `pixi_glob`
- `pixi_path`
- `fs-err`
- `ignore` if needed for lower-level traversal/matching
- `thiserror`
- dev-dependencies: `criterion`, `globwalk`, `tempfile`

Implementation expectations:

- `PathMetadataKey` performs a fresh `metadata`/`symlink_metadata` read when computed.
- `ReadDirKey` reads and returns deterministic directory entries.
- `GlobSnapshotKey` computes through `ReadDirKey` and `PathMetadataKey`; avoid bypassing the compute engine for filesystem inputs.
- `GlobSnapshotKey` should preserve Pixi glob semantics as much as practical. Reuse `pixi_glob` semantics or port matcher logic carefully.
- `GlobFreshnessKey` compares a previous snapshot to the current snapshot and returns a clear stale reason.
- `Key::equality` for filesystem keys should compare semantic state (`FileState`, sorted dir entries, sorted snapshot entries), not pointer identity.

### Phase 3: Benchmarks

Add synthetic fixture generation in benchmark code so benchmarks are repeatable.

Benchmark groups should separate:

- cold full walk/snapshot;
- warm repeated snapshot/freshness;
- changed FS key commit + recompute after small changes.

### Phase 4: Validation and documentation

Run focused validation as far as implemented:

```bash
cargo test -p pixi_compute_engine
cargo test -p pixi_compute_fs
cargo test -p pixi_glob
cargo bench -p pixi_compute_fs
```

If full benches are too slow or `pixi_compute_fs` is deferred, document exactly what was skipped and why.

## Acceptance criteria

Minimum acceptance if this session stops after compute-engine redesign:

- Handoff docs are updated to DICE-style semantics.
- Source tree state is restored or a safe restoration blocker is reported.
- `pixi_compute_engine` has the first DICE-style snapshot/invalidation slice with tests, or a precise stop report explaining why the redesign must be split.
- Removal-based invalidation is not implemented as the primary architecture.

Full acceptance for the combined milestone:

- `pixi_compute_engine` supports DICE-style snapshots/transactions, active versions, dirty/vacant invalidation, rdeps, equality/backdating, and changed-to/reinject semantics.
- `pixi_compute_fs` crate exists and builds.
- `PathMetadataKey`, `ReadDirKey`, `GlobSnapshotKey`, and `GlobFreshnessKey` exist.
- `GlobSnapshotKey` and `GlobFreshnessKey` route filesystem inputs through compute-engine keys.
- Criterion benchmarks exist and compare against at least current `pixi_glob` and `globwalk`.
- No daemon/IPC/watcher implementation is added.
- Any deferred pieces are explicitly documented.

## Stop rules

Stop and ask/report if:

- source restoration cannot be done without risking user changes;
- implementing DICE-style snapshots requires an unbounded rewrite of `pixi_compute_engine` that should be split into a separate PR/milestone;
- `AnyKey`/type erasure cannot support versioned graph state without changing core trait bounds substantially;
- graph-owner serialization conflicts with current executor/runtime assumptions;
- preserving existing `pixi_glob` semantics would require a large rewrite;
- benchmarks show the cold path is dramatically slower than current `pixi_glob` and the cause is architectural rather than incidental.

## Final report shape

When done, report:

- changed files;
- source restoration actions;
- implemented compute-engine snapshot/invalidation APIs;
- concurrency semantics and what is/is not guaranteed;
- `pixi_compute_fs` public API summary if implemented;
- benchmark results or how to run them if implemented;
- validation commands and outcomes;
- deferred risks/open questions;
- next recommended milestone, likely the per-workspace daemon from `design/pixi-daemon-followup-handoff.md` after `pixi_compute_fs` lands.

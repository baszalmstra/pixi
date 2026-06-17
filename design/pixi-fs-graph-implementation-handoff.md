# Pixi filesystem graph implementation handoff

## Goal

Build the filesystem graph on top of a DICE-style compute-engine substrate.

The implementation order is now:

1. Restore/verify the source tree.
2. Implement DICE-style compute snapshots/invalidation in `pixi_compute_engine`.
3. Add `pixi_compute_fs` with filesystem keys and glob snapshot/freshness APIs.
4. Add benchmarks.

The daemon itself is **not** part of this milestone.

## Problem being solved

Large Pixi workspaces pay repeated per-invocation filesystem costs even when nothing changed. Source package freshness is the first production-shaped slice: backend metadata freshness currently requires repeated glob walks and file metadata checks to prove nothing changed.

The target model is:

> Model filesystem state and glob freshness as compute-engine computations, so a future daemon can keep them warm, serve concurrent requests from graph-version snapshots, and commit filesystem changes to newer graph versions.

## Core design rules

1. All filesystem inputs to cached computations must flow through the compute engine.
2. Watcher events must only mark low-level filesystem keys changed/dirty.
3. Derived glob results must become stale through compute-engine dependencies.
4. Requests run against compute snapshots/transactions.
5. Snapshots are read-only with respect to invalidation/reinjection.
6. Concurrent filesystem updates create newer graph versions; they do not mutate active snapshots.
7. We require graph-version consistency, not true historical filesystem snapshots.

## Required pre-implementation gate

The primary compute-engine design is now:

- `daemon-research/invalidation-research-mini-design.md`

Treat older removal-based invalidation notes as historical context only.

## Milestone scope

### In scope: `pixi_compute_engine`

Implement the first DICE-style foundation needed by filesystem graph keys and future daemon concurrency:

- `ComputeEngine::snapshot()` or equivalent transaction API.
- Read-only snapshot compute handle.
- Master-only changed/invalidate APIs.
- Active graph versions with refcount cleanup.
- Per-version task cache for in-flight deduplication.
- Versioned graph nodes, not deletion-based invalidation.
- Dirty/vacant markers for missing invalidated keys.
- Reverse dependencies and transitive dirty propagation.
- `changed_to`/`re_inject` semantics for injected/replaced values.
- `Key::equality` wired for DICE-style change pruning/backdating.
- Tests for concurrency and invalidation semantics.

Recommended implementation style: a single graph-owner task/thread serializes graph mutation and version bookkeeping, while workers compute concurrently.

### In scope: `pixi_compute_fs`

Add a new reusable crate, preferably named `pixi_compute_fs`, with compute-engine keys and value types for filesystem state:

- `PathMetadataKey(path)`
- `ReadDirKey(dir)`
- `GlobSnapshotKey { root, patterns, markers, exclude_hidden }`
- `GlobFreshnessKey { glob_spec, previous_snapshot }`

Initial file state can be metadata based:

- file type
- size
- mtime
- inode / ctime where available and practical

Design the value shape so digest-based freshness can be added later without changing the graph architecture.

### In scope: benchmarks

Add Criterion benchmarks from day one once `pixi_compute_fs` exists. Benchmark at least:

- current `pixi_glob`
- `globwalk`
- raw `ignore` walker if useful
- cold `pixi_compute_fs::GlobSnapshotKey`
- warm `pixi_compute_fs::GlobSnapshotKey` / `GlobFreshnessKey`
- changed-key commit + recompute for small changes

### Out of scope

- No daemon process.
- No IPC.
- No filesystem watcher implementation.
- No `pixi run` integration.
- No lockfile/activation/prefix state moved into the daemon.
- No broad semantic invalidation for all Pixi compute keys beyond the compute substrate.
- No remote or privileged service.
- No true historical filesystem snapshot semantics.

## Architecture sketch

Future daemon flow:

```text
request A
  -> engine.snapshot() at v10
  -> compute command data from v10 graph

watcher event while A runs
  -> engine.changed(PathMetadataKey(file))
  -> graph owner commits v11 and dirties rdeps for future snapshots

request B
  -> engine.snapshot() at v11
  -> sees v11 graph state
```

Filesystem graph path:

```text
PathMetadataKey(path) -> FileState
ReadDirKey(dir)       -> sorted directory entries

GlobSnapshotKey(spec)
  -> depends on ReadDirKey for visited dirs
  -> depends on PathMetadataKey for matched files
  -> returns matched files + metadata snapshot

GlobFreshnessKey(spec, previous_snapshot)
  -> computes current snapshot or affected subset
  -> returns Fresh | Stale(reason)
```

Future watcher behavior:

```text
file modified
  -> changed PathMetadataKey(file)

file created/deleted/renamed
  -> changed ReadDirKey(parent)
  -> changed PathMetadataKey(file)

missed watcher events / overflow
  -> invalidate_all / unstable_drop_everything or fs-key-type scoped reset
```

## Implementation notes

### `pixi_compute_engine`

Relevant files after source restoration:

- `crates/pixi_compute_engine/src/key_graph.rs`
- `crates/pixi_compute_engine/src/engine.rs`
- `crates/pixi_compute_engine/src/ctx.rs`
- `crates/pixi_compute_engine/src/key.rs`
- `crates/pixi_compute_engine/src/injected.rs`
- `crates/pixi_compute_engine/src/any_key.rs`

Existing facts from prior research:

- Completed nodes currently store forward dependencies: `deps: Vec<AnyKey>`.
- No reverse deps exist today.
- Injected keys are write-once because invalidation does not exist.
- `Key::equality` exists for early cutoff but is not wired.
- The current in-flight generation guard may help reject stale completions, but DICE-style snapshots need version/epoch-level rejection.

Expected compute-engine tests:

- Snapshot at old version continues while newer invalidation commits.
- Snapshot does not expose invalidation APIs.
- Concurrent snapshots at same version share in-flight work.
- Active-version refcount cleanup drops/cancels per-version task cache.
- Dirtying a leaf dirties direct and transitive rdeps.
- Dirtying a missing key creates a vacant dirty marker.
- Direct force-dirty normal key recomputes even if deps are unchanged.
- Equal recomputation + equal deps backdates/marks unchanged.
- `changed_to` equal value is no-op for dependents.
- `changed_to` changed value dirties dependents.
- `invalidate_all`/reset behavior is explicit and tested.

### `pixi_compute_fs`

Suggested crate dependencies:

- `pixi_compute_engine`
- `pixi_glob` for existing semantics or helpers
- `pixi_path`
- `fs-err`
- `ignore` if lower-level matcher/walker access is needed
- `thiserror`
- `criterion` as dev-dependency for benches
- `globwalk` as dev-dependency for benchmark comparison

Suggested module structure:

```text
crates/pixi_compute_fs/
  Cargo.toml
  benches/
    glob_snapshot.rs
  src/
    lib.rs
    file_state.rs
    keys/
      mod.rs
      path_metadata.rs
      read_dir.rs
      glob_snapshot.rs
      glob_freshness.rs
    glob_spec.rs
    snapshot.rs
```

Suggested public API shape:

```rust
pub struct FileState { ... }
pub struct DirEntryState { ... }
pub struct GlobSpec { root, patterns, markers, exclude_hidden }
pub struct GlobSnapshot { files, fingerprint/generation metadata }
pub enum GlobFreshness { Fresh, Stale { reason: StaleReason } }
```

`GlobSnapshotKey` should be the primitive. `GlobFreshnessKey` should be a thin derived computation on top.

## Benchmark requirements

Benchmark scenarios:

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
- directory deleted/renamed

Performance gates:

1. Cold path: `pixi_compute_fs::GlobSnapshotKey` should be in the same broad performance class as current `pixi_glob` / `globwalk`.
2. Warm path: repeated snapshot/freshness should avoid full tree walking when no relevant dirty keys were committed.
3. Small change path: dirtying a small number of low-level filesystem keys should recompute only affected derived work where equality/deps permit.

## Validation commands

Run as far as the implemented scope allows:

```bash
cargo test -p pixi_compute_engine
cargo test -p pixi_compute_fs
cargo test -p pixi_glob
cargo bench -p pixi_compute_fs
```

If the compute-engine redesign becomes the whole session, document that `pixi_compute_fs` and benches are deferred.

## Stop conditions

Stop and ask/report if:

- the source tree cannot be restored safely;
- DICE-style graph-owner/versioned-node work is too large for a single coherent change;
- type erasure (`AnyKey`) blocks safe rdep/versioned node handling;
- current executor assumptions conflict with a graph-owner task/thread;
- preserving `pixi_glob` semantics requires a separate design pass.

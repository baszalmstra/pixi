# Minimal State Slice: Source/Lock Freshness for `pixi run` Wins

## Scope

Identify the narrowest daemon-warmable state subset on the `pixi run` freshness
path where fine-grained invalidation produces visible latency wins *without*
having to solve full daemon invalidation of all memoized compute keys.

## Files Inspected

1. `docs/crates/pixi_core/src/lock_file/update.rs` (lines 223-340, 617-665, 707-893)
   Entry point: `Workspace::update_lock_file`, `UpdateMode`, prefix quick-validate path.
2. `docs/crates/pixi_core/src/lock_file/outdated.rs` (lines 67-220, 252-490)
   `OutdatedEnvironments` freshness inventory and `find_unsatisfiable_targets`.
3. `docs/crates/pixi_core/src/lock_file/satisfiability/platform.rs` (lines 150-420)
   `verify_platform_satisfiability` – source-record re-resolution and backend calls.
4. `docs/crates/pixi_core/src/lock_file/satisfiability/source_record.rs` (lines 1-227)
   `verify_partial_source_record_against_backend` – the per-source freshness gate.
5. `docs/crates/pixi_command_dispatcher/src/build_backend_metadata/mod.rs` (lines 70-460, 690-1076)
   `BuildBackendMetadataInnerKey::compute` – cache probe, freshness, glob walks, backend RPC.
6. `docs/crates/pixi_command_dispatcher/src/cache/backend_metadata.rs` (lines 1-270)
   On-disk `BuildBackendMetadataCacheEntry` schema and freshness inputs.
7. `docs/crates/pixi_command_dispatcher/src/input_globs.rs` (lines 1-86)
   `collect_input_files` – glob walks for mutable source freshness.
8. `docs/crates/pixi_command_dispatcher/src/input_hash.rs` (lines 1-96)
   `ProjectModelHash`, `ConfigurationHash`, `BackendSpecHash`, `BackendBinaryFingerprint`.
9. `docs/crates/pixi_core/src/lock_file/satisfiability/pypi.rs` (lines 128-230, 525-610)
   PyPI requirement matching and static metadata cache.
10. `docs/crates/pixi_core/src/lock_file/satisfiability/pypi_metadata.rs` (lines 1-110)
    `LocalPackageMetadata` comparison struct.
11. `docs/crates/pixi_core/src/environment/mod.rs` (lines 122, 219-258, 585-625)
    `LockedEnvironmentHash` and `LockFileUsage`.
12. `docs/crates/pixi_record/src/source_record.rs` (lines 366-410)
    `has_mutable_source()` / `is_partial()` – the gating predicates.
13. `docs/crates/pixi_record/src/pinned_source.rs` (lines 100-140)
    `PinnedSourceSpec::is_mutable()` → true for `PinnedSourceSpec::Path(_)`.

## What `pixi run` Actually Does

```
pixi run <task>
  └─ Workspace::update_lock_file(mode=Update)
       ├─ load & parse pixi.lock                         [cold: file I/O]
       ├─ build LockFileResolver (once per call)          [cold: parse over lock]
       ├─ build fresh CommandDispatcher + GlobHashCache   [cold: always new]
       ├─ OutdatedEnvironments::from_workspace_and_lock_file
       │    ├─ verify_environment_satisfiability (per env) → fast config diffs
       │    └─ verify_platform_satisfiability (per env×platform)
       │         ├─ for immutable full source records → trust, skip backend
       │         └─ for mutable/partial source records →
       │              verify_partial_source_record_against_backend
       │                └─ BuildBackendMetadataKey::compute
       │                     └─ BuildBackendMetadataInnerKey::compute
       │                          ├─ probe_cache (read on-disk cache)
       │                          │    ├─ check project_model_hash
       │                          │    ├─ check configuration_hash
       │                          │    ├─ check backend_spec_hash
       │                          │    ├─ check backend_binary_fingerprint
       │                          │    ├─ check build_variants
       │                          │    ├─ for mutable: check file mtmes × recorded files
       │                          │    └─ for mutable: collect_input_files (glob walk!)
       │                          └─ on miss/stale: instantiate backend, RPC conda/outputs
       └─ if outdated → full solve/update (expensive)
          if not outdated → return early (FAST)
```

## Where Invalidation Matters Most

### 1. Mutable-source backend metadata cache freshness (THE KEY SLICE)

**Why it's expensive on every `pixi run`:**
- Workspaces with local-path source packages (`PinnedSourceSpec::Path`) are
  classified as *mutable*. During `verify_platform_satisfiability`, mutable
  source records always call `verify_partial_source_record_against_backend`.
- This triggers `BuildBackendMetadataInnerKey::compute`, which probes the
  on-disk metadata cache.
- Even on a **cache hit** (nothing changed), `verify_cache_freshness` must:
  - Stat every recorded `input_file` to check its mtime (`390-425` in
    `build_backend_metadata/mod.rs`).
  - Execute `collect_input_files` – a full **glob walk** over the source tree
    to detect newly added files matching backend-reported globs (`407-420`).
- These filesystem walks are the dominant latency in the "nothing changed" case.

**What gets checked (the invalidation inputs):**

| Input | Source | When checked |
|-------|--------|--------------|
| `project_model_hash` | Hashed from backend init `project_model` | Always |
| `configuration_hash` | Hashed from `[package.build.config]` | Always |
| `backend_spec_hash` | Hashed from `BackendSpec` (name + version + channels) | Always |
| `backend_binary_fingerprint` | xxh3 of backend executable (system/path backends) | Always |
| `build_variants` | `BTreeMap<String, Vec<VariantValue>>` | Always |
| `build_variant_files` | Paths from variant file config | Mutable only |
| Recorded `input_files` mtimes | Cache entry's stored absolute file paths | Mutable only |
| New files matching `input_glob_sets` | Glob walk against source tree root | Mutable only |

**Current daemon gap:**
- `CommandDispatcher::clear_filesystem_caches()` only clears `GlobHashCache`.
- `InputGlobSetWalkKey` results are memoized in the `ComputeEngine` for the
  dispatcher's lifetime with **no invalidation mechanism** beyond engine teardown.
- The on-disk `BuildBackendMetadataCache` entries have their own self-contained
  freshness, but the *probe* itself (file mtime checks + glob walk) is
  repeated every time.

**Recommended invalidation inputs for this slice:**
- Filesystem watch on the source tree roots (the `build_source_dir` used in
  glob walks). Events: create, modify, delete.
- Watch on variant files (`build_variant_files` paths).
- Watch on backend executable path (for system/path backends with fingerprint).
- Watch on manifest / `pixi.toml` (for `project_model_hash` and
  `configuration_hash` changes).
- Channel list changes, exclude-newer changes, virtual package changes
  (for build environment identity).
- Backend override changes (`BackendOverride` config).

### 2. Lock-file resolver and parsed lock state

**Why it matters:**
- `LockFileResolver::build` parses the lock file into an indexed in-memory
  structure. Built once per `update_lock_file` call.
- The `LockFile` itself is parsed from disk each time.

**Recommended invalidation inputs:**
- `pixi.lock` file content/mtime/hash.

### 3. Prefix quick-validation bypass for source packages

**Why it's important:**
- At `update.rs:845-893`, `cached_prefix` deliberately **skips** the
  `LockedEnvironmentHash` quick path when the lock file contains conda source
  packages or PyPI directory source deps. This forces a full prefix install
  check on every `pixi run` for workspaces with source packages.
- A daemon that can prove source-package state is unchanged could avoid this
  forced re-check.

### 4. Static PyPI metadata cache

**What it does:**
- `read_local_package_metadata` caches `pyproject.toml` metadata per
  directory path in `static_metadata_cache`.
- Currently per-invocation only; shared across platforms within one call.
- Compares `version`, `requires_dist`, `requires_python` against locked values.

**Recommended invalidation inputs:**
- File watch on each cached local package directory's `pyproject.toml`.

## Recommended First Slice

**Daemon-warm the `BuildBackendMetadataCache` in-memory index with
file-watch-driven freshness tracking.**

This is the narrowest slice because:

1. **It targets the single most expensive repeated check** for workspaces
   with local source packages. The glob walk and per-file mtime checks
   dominate the "nothing changed" `pixi run` latency.

2. **It has a clean on-disk cache already** – the `BuildBackendMetadataCache`
   stores all needed fields (revision, hashes, input files, input glob sets,
   timestamp, outputs). A daemon can index these entries in memory and serve
   freshness verdicts without touching the filesystem.

3. **Invalidation is tractable** – file watching on the source tree root
   plus the few config inputs effectively covers it.

4. **It avoids solving the full daemon problem** – it doesn't require
   warming the entire `ComputeEngine`, `GlobHashCache`, solve results, or
   prefix install state. It's a single cache layer with well-defined
   invalidation edges.

5. **Measurable `pixi run` win**: For a workspace with path-based source
   packages and nothing changed, the glob walk and file stat loop are
   eliminated entirely. The daemon can answer the freshness question from
   memory and a minimal "has any watched file changed?" check.

### What the slice does NOT cover (and can be added later)

- Warming the `LockFileResolver` (minor parse cost).
- Warming conda/PyPI solve results.
- Warming prefix install state.
- Warming `GlobHashCache` for task input hashing.
- In-process `InputGlobSetWalkKey` deduplication across calls.

### Invalidation inputs for this slice

```rust
// Invalidation key for a single BuildBackendMetadata cache entry
struct BackendMetadataFreshnessKey {
    // From BuildBackendMetadataCacheKey (identity):
    channel_urls: Vec<ChannelUrl>,
    build_environment: BuildEnvironment,  // platforms + virtual packages
    exclude_newer: Option<ResolvedExcludeNewer>,
    enabled_protocols: EnabledProtocols,
    source: CanonicalSourceCodeLocation,

    // From cache entry (freshness signals):
    project_model_hash: Option<ProjectModelHash>,
    configuration_hash: ConfigurationHash,
    backend_spec_hash: Option<BackendSpecHash>,
    backend_binary_fingerprint: Option<BackendBinaryFingerprint>,
    build_variants: BTreeMap<String, Vec<VariantValue>>,
}

// File-watch targets per entry:
// - source_tree_root: the build_source_dir (mutable sources only)
// - variant_files: the build_variant_files paths
// - backend_binary_path: for system/path backends with fingerprint
// - manifest_path: the workspace pixi.toml (affects project_model/config)
```

### State to keep warm

1. **In-memory index** of all `BuildBackendMetadataCacheEntry` records keyed by
   `BuildBackendMetadataCacheKey`, including their `revision`, all hashes,
   `input_files` (absolute paths), `input_glob_sets`, `outputs`, and
   `timestamp`.

2. **File-watch subscription map**: for each mutable source entry, map the
   source tree root + variant file paths → affected cache keys.

3. **Watcher-driven staleness flag**: on any create/modify/delete under a
   watched root, mark affected cache keys as stale. Stale entries require
   re-probing (glob walk to find new files + mtime checks on recorded files).

4. **Config-change listeners**: on channel/platform/virtual-package/
   exclude-newer/backend-override/protocol changes, mark affected keys stale
   or invalidate the entire index.

### Start Here

Open `docs/crates/pixi_command_dispatcher/src/build_backend_metadata/mod.rs`
at `verify_cache_freshness` (line ~295). This function is the exact freshness
contract the daemon must replicate. Then open
`docs/crates/pixi_command_dispatcher/src/cache/backend_metadata.rs` to see
the on-disk schema the daemon would index in memory.

# Code Context

## Files Retrieved
1. `docs/crates/pixi_command_dispatcher/src/keys/source_metadata.rs` (lines 1-207) - `SourceMetadataKey` pins/checks out a source and delegates to build-backend metadata.
2. `docs/crates/pixi_command_dispatcher/src/dev_source_metadata/mod.rs` (lines 1-260) - `DevSourceMetadataKey` derives dev-source records from backend metadata.
3. `docs/crates/pixi_command_dispatcher/src/build_backend_metadata/mod.rs` (lines 1-224, 281-611, 760-1076) - backend metadata specs, cache strategy, freshness checks, glob collection, and cache writes.
4. `docs/crates/pixi_command_dispatcher/src/cache/backend_metadata.rs` (lines 1-270) - on-disk metadata cache key/entry schema and freshness state.
5. `docs/crates/pixi_command_dispatcher/src/input_globs.rs` (lines 1-130) and `docs/crates/pixi_command_dispatcher/src/keys/input_glob_set_walk.rs` (lines 1-133) - normalized backend globs and compute-engine glob-walk key.
6. `docs/crates/pixi_command_dispatcher/src/input_hash.rs` (lines 1-90), `docs/crates/pixi_glob/src/glob_hash_cache.rs` (lines 1-107) - stable hashes/fingerprints and in-process glob hash cache.
7. `docs/crates/pixi_command_dispatcher/src/keys/resolve_source_package.rs` (lines 1-230) and `docs/crates/pixi_command_dispatcher/src/keys/resolve_source_record.rs` (lines 1-220) - source metadata is expanded into full source records with nested build/host solves.
8. `docs/crates/pixi_command_dispatcher/src/keys/source_build.rs` (lines 1-220, 221-480) - source build key, artifact/workspace cache use, build-input glob sidecar state.
9. `docs/crates/pixi_command_dispatcher/src/cache/artifact.rs` (lines 1-260, 261-428) and `docs/crates/pixi_command_dispatcher/src/cache/workspace.rs` (lines 1-170) - source artifact/workspace cache keying and freshness checks.
10. `docs/crates/pixi_command_dispatcher/src/install_pixi/ext.rs` (lines 95-200) - install path splits source records, clears source caches on force reinstall, fans out `SourceBuildKey`.
11. `docs/crates/pixi_command_dispatcher/src/command_dispatcher/mod.rs` (lines 100-309, 340-388) - dispatcher-owned caches, public source metadata methods, filesystem-cache clearing.
12. `docs/crates/pixi_core/src/environment/mod.rs` (lines 218-260) and `docs/crates/pixi_core/src/lock_file/update.rs` (lines 846-888) - prefix quick-validation hash and source-package bypass.

## Key Code

### Dispatcher/source metadata keys
- `CommandDispatcher` owns a long-lived `ComputeEngine`, `BuildBackendMetadataCache`, `GlobHashCache`, source resolvers, package cache, and workspace env registry (`command_dispatcher/mod.rs:100-309`). It exposes:
  - `build_backend_metadata(spec)` as a thin `BuildBackendMetadataKey` wrapper (`command_dispatcher/mod.rs:346-359`).
  - `dev_source_metadata(spec)` as a `DevSourceMetadataKey` wrapper (`command_dispatcher/mod.rs:361-384`).
  - `clear_filesystem_caches()` currently only clears `GlobHashCache` (`command_dispatcher/mod.rs:303-309`).
- `SourceMetadataSpec` key identity is package, unpinned `source_location`, optional preferred build source, optional manifest pin override, and `EnvironmentRef` (`keys/source_metadata.rs:72-98`). `SourceMetadataKey::compute` pins/checks out the manifest source, builds `BuildBackendMetadataSpec`, computes `BuildBackendMetadataKey`, filters `CondaOutput`s by package, and returns `SourceOutputs` (`keys/source_metadata.rs:137-207`).
- `ResolveSourcePackageKey` wraps `SourceMetadataKey`, reuses installed source hints to avoid git branch drift, and fans out `assemble_source_record` per output variant (`keys/resolve_source_package.rs:76-195`).
- `DevSourceMetadataKey` computes `BuildBackendMetadataKey` directly and converts matching outputs into `DevSourceRecord`s (`dev_source_metadata/mod.rs:123-178`).

### Build-backend metadata cache/freshness
- Public `BuildBackendMetadataSpec` includes pinned manifest source, optional preferred build source, `EnvironmentRef`, and build string/number overrides (`build_backend_metadata/mod.rs:74-106`). Outer key resolves env projections: channels, build env, variants, exclude-newer (`build_backend_metadata/mod.rs:126-159`).
- Inner identity/freshness inputs include pinned sources, channels, build env, variant config/files, exclude-newer, build overrides (`build_backend_metadata/mod.rs:164-204`).
- On-disk cache key includes channel URLs, build environment/platform+virtual packages, exclude-newer, enabled protocols, and canonical source location (`cache/backend_metadata.rs:52-126`).
- Cache entry stores `revision`, `cache_version`, `project_model_hash`, `configuration_hash`, `backend_spec_hash`, optional `backend_binary_fingerprint`, canonical source, build variants/files, `input_glob_sets`, absolute `input_files`, timestamp, and backend outputs (`cache/backend_metadata.rs:138-232`).
- Freshness checks invalidate on project model/config/backend spec/backend binary fingerprint/variant changes, then for mutable sources on modified/deleted recorded input files or newly matched glob files (`build_backend_metadata/mod.rs:289-426`). Immutable source checkouts skip file freshness checks (`build_backend_metadata/mod.rs:375-378`).
- Backend cache strategy: package-resolved backends are cacheable by spec hash; system/path backends add a binary fingerprint; mutable env backends or unresolved system commands skip cache (`build_backend_metadata/mod.rs:227-285`, `build_backend_metadata/mod.rs:781-812`).
- Backend RPC result normalizes flat and structured input globs, walks them, stores absolute input files and timestamp (`build_backend_metadata/mod.rs:524-575`, `build_backend_metadata/mod.rs:992-1059`). If outputs are unchanged from a stale entry, the old `revision` is reused; otherwise a new revision is created (`build_backend_metadata/mod.rs:1009-1021`).

### Input glob/hash state
- `fold_input_globs` converts backend `input_globs` plus optional structured `input_glob_sets` into one vector of `InputGlobSet`s; flat globs become a default marker-free hidden-excluding group (`input_globs.rs:21-40`).
- `collect_input_files` uses `InputGlobSetWalkKey` per group through the compute engine and dedups absolute paths (`input_globs.rs:42-72`).
- `InputGlobSetWalkKey` identity is resolved root, ordered patterns, sorted markers, and `exclude_hidden`; values are cached by the compute engine for the dispatcher lifetime (`keys/input_glob_set_walk.rs:21-133`). This is filesystem-sensitive but no explicit invalidation exists except dropping/clearing the engine.
- `GlobHashCache` is a separate in-process cache for content hashes keyed by `(root, globs)`; it assumes input files do not change between calls and has an explicit `clear()` (`pixi_glob/src/glob_hash_cache.rs:1-107`). In this source-metadata path, freshness uses glob walks/mtimes, not `GlobHashCache`; `GlobHashCache` is still dispatcher state a daemon must invalidate on filesystem changes.
- Hashes in `input_hash.rs` cover project model, backend spec, backend executable contents, and build configuration (`input_hash.rs:1-90`).

### Source build/cache/workspace state
- `SourceBuildSpec` key includes the unresolved source record with populated build/host packages, channels, exclude-newer, build env/profile, variants/files, build overrides, and package format (`keys/source_build.rs:55-101`).
- Artifact cache key hashes package name, pinned manifest/build sources, variants, build+host platforms, backend identifier, build overrides, package format, binary dep URLs+sha256s, and recursive source dep artifact sha256s; source files are deliberately not in the key (`cache/artifact.rs:42-116`, `keys/source_build.rs:183-201`).
- Artifact freshness lives in `sidecar.json`: input glob sets, absolute input files with mtimes, artifact sha256, artifact filename, and synthesized `RepoDataRecord` (`cache/artifact.rs:118-159`). `lookup` invalidates on missing/unparseable sidecar, changed/missing input file mtime, newly matched glob file, or missing artifact (`cache/artifact.rs:261-331`). `store` captures current mtimes and writes sidecar (`cache/artifact.rs:333-428`).
- Workspace cache key hashes package, pinned sources, variants, build/host platforms, backend identifier, and full build/host package lists; package format is intentionally excluded so different encodings share build state (`cache/workspace.rs:1-78`, `keys/source_build.rs:246-264`). Workspaces are exclusively locked per package/key (`cache/workspace.rs:99-127`).
- On install, source records are separated from binaries, source package caches are cleared for forced reinstall, then source packages build concurrently with `SourceBuildKey`; ignored source packages are not built (`install_pixi/ext.rs:95-200`).

### Prefix quick-validation impact
- `LockedEnvironmentHash` quick validation hashes locked package location plus conda sha/md5 (PyPI package details are ignored) and ignores skipped packages (`environment/mod.rs:218-252`).
- `LockFileUsage::cached_prefix` accepts a prefix when `pixi_env.json` hash matches, except if the lock file/environment contains conda source packages or directory PyPI source dependencies; then it deliberately ignores the hash and updates the prefix (`lock_file/update.rs:846-888`). This prevents accepting stale editable/source-built content based only on lock hash.

## Architecture

Source metadata flows through compute-engine keys:

`SolvePixiEnvironmentKey` -> `ResolveSourcePackageKey` -> `SourceMetadataKey` -> `BuildBackendMetadataKey` -> `BuildBackendMetadataInnerKey` -> source checkout/backend discovery/backend RPC/cache.

`BuildBackendMetadataKey` is the main dispatcher key for expensive metadata. It resolves environment projections, pins sources, checks backend metadata disk cache, calls `conda/outputs` only on cache miss/stale/skip-cache, records input globs/files, and writes a revisioned entry. `SourceMetadataKey` only filters outputs for a package; `ResolveSourcePackageKey` then performs nested build/host solves to assemble full source records. Dev sources use the same backend metadata but produce `DevSourceRecord`s without building the package.

Install/build flow is separate but shares source records and glob machinery:

`InstallPixiEnvironmentKey`/install path -> split source records -> `SourceBuildKey` -> recursive source dep builds -> artifact cache lookup -> backend build on miss -> workspace cache for incremental build state -> artifact sidecar store.

## Daemon watchable/in-memory state and invalidation inputs

A daemon could keep warm:
- A long-lived `CommandDispatcher` + `ComputeEngine` with memoized `SourceMetadataKey`, `BuildBackendMetadataKey`, `InputGlobSetWalkKey`, source checkout, backend discovery/instantiation, and solve/build keys.
- On-disk `BuildBackendMetadataCacheEntry` indexes in memory: cache key, revision/cache_version, hashes, variants/files, input globs/files, timestamp, outputs.
- Source-build artifact sidecar indexes: package/cache key, input glob sets/files+mtimes, artifact sha256/record/path.
- Workspace cache directories/locks and backend identifiers/processes where safe.
- `GlobHashCache` entries if daemon also serves task/input-hash operations.

Invalidation inputs:
- Workspace manifest/package recipe files and any backend-reported metadata/build input files; additions matching stored input globs matter, not just modifications/deletions.
- `InputGlobSetWalkKey` roots/patterns/markers/hidden policy; any watched create/delete/modify under watched roots should invalidate affected glob-walk keys and downstream metadata/build artifact freshness.
- Variant files and variant configuration, channels, build/host platforms and virtual packages, exclude-newer, enabled protocols, backend overrides, build string/number overrides.
- Backend discovery inputs/project model/configuration/target config/backend spec; backend executable contents for system/path backends; mutable env backends should remain skip-cache or get coarse invalidation.
- Source location pins: git/url branch/ref resolution, pinned manifest source, preferred build source, build source declared by manifest; path sources are mutable and need FS watching.
- Build/host dependency records and source dependency artifact sha256s for `SourceBuildKey` artifact/workspace keys.
- `.pixi/envs/**/conda-meta/pixi_env.json` and prefix contents for prefix quick-validation; source packages should continue to force prefix update unless daemon can prove source-built/editable state is fresh.
- Manual invalidators: force reinstall/clean calls clear per-package source artifact/workspace caches; daemon must mirror or observe these deletions.

Risk/open point: compute-engine memoized `InputGlobSetWalkKey` and source metadata results are correct only for a static filesystem. A daemon needs explicit graph invalidation or coarse engine/session reset on source-tree changes; clearing only `GlobHashCache` is insufficient for source metadata/build glob walks.

## Start Here
Open `docs/crates/pixi_command_dispatcher/src/build_backend_metadata/mod.rs` first. It defines the source metadata freshness contract and all inputs that decide whether pixi can reuse backend metadata or must call the backend again.

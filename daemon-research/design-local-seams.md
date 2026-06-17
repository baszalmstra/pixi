# Code Context

## Files Retrieved
1. `design/pixi-daemon.md` (lines 1-28) - daemon motivation: Pixi pays stateless disk-rebuild costs per CLI invocation.
2. `crates/pixi_core/src/workspace/mod.rs` at commit `3b14a489...` (lines 137-191, 346-417, 783-848) - `Workspace` shape, manifest/config loading, and per-workspace `CommandDispatcherBuilder` seam.
3. `crates/pixi_core/src/workspace/discovery.rs` at commit `3b14a489...` (lines 67-78, 202-310) - CLI workspace discovery flow.
4. `crates/pixi_manifest/src/discovery.rs` at commit `3b14a489...` (lines 32-55, 73-130) - manifest read/parse entry point.
5. `crates/pixi_core/src/lock_file/update.rs` at commit `3b14a489...` (lines 236-335, 426-470, 561-612, 633-666, 770-815, 845-893) - lockfile load/outdated check, derived-data caches, and prefix quick validation.
6. `crates/pixi_core/src/lock_file/outdated.rs` at commit `3b14a489...` (lines 36-100, 125-145, 220-250) - lock satisfiability/outdated state and temporary metadata caches.
7. `crates/pixi_core/src/environment/mod.rs` at commit `3b14a489...` (lines 314-410, 651-790) - environment marker file and lock+prefix update API.
8. `crates/pixi_compute_engine/src/engine.rs` at commit `3b14a489...` (lines 85-179, 303-324) and `crates/pixi_compute_engine/src/key.rs` (lines 11-27, 107-130) - in-memory compute engine semantics.
9. `crates/pixi_compute_engine/src/injected.rs` at commit `3b14a489...` (lines 18-43) - explicit lack of injected-value invalidation.
10. `crates/pixi_command_dispatcher/src/command_dispatcher/mod.rs` at commit `3b14a489...` (lines 51-65, 91-120, 182-310) and `builder.rs` (lines 346-363, 421-547) - dispatcher owns compute engine, filesystem caches, injected snapshots.
11. `crates/pixi_command_dispatcher/src/keys/input_glob_set_walk.rs` at commit `3b14a489...` (lines 1-9, 21-38, 92-139), `input_hash.rs` (lines 21-63), `keys/source_metadata.rs` (lines 72-98, 111-160), `build_backend_metadata/mod.rs` (lines 220-260) - source/build filesystem dependency surfaces.
12. `crates/pixi_cli/src/run.rs` at commit `3b14a489...` (lines 124-143, 223-279, 402-455) and `crates/pixi_cli/src/shell.rs` (lines 324-365) - CLI orchestration that should mostly stay client-side.

## Key Code

- `WorkspaceLocator::for_cli().locate()` discovers and parses manifests on every invocation, then `Workspace::from_manifests` reloads config and initializes per-process caches (`Workspace` has `OnceCell` clients/gateway and `env_vars`) (`workspace/discovery.rs:202-310`, `workspace/mod.rs:346-417`).
- `Workspace::update_lock_file()` always starts by reading `pixi.lock`, creates a fresh `CommandDispatcher`, wraps a fresh `LockFileDerivedData`, then checks `OutdatedEnvironments` unless `--frozen`/skip mode says not to (`lock_file/update.rs:236-318`).
- `LockFileDerivedData` already identifies reusable state for one invocation: lock resolver, command dispatcher, prefix-update cells, uv context, package cache, glob hash cache, and per-env/platform build caches (`lock_file/update.rs:561-612`, `633-666`). This is a strong candidate daemon session object.
- Prefix freshness is currently disk-marker based: `prefix()` computes a `LockedEnvironmentHash`, checks `conda-meta/pixi_env.json`, and skips only if hashes match and no source packages are present (`lock_file/update.rs:770-815`, `845-893`; `environment/mod.rs:314-410`).
- `CommandDispatcher` wraps a `ComputeEngine` plus shared cache/network/build resources (`command_dispatcher/mod.rs:51-65`, `91-120`). Builder snapshots environment variables and injects immutable keys (`CacheDirsKey`, `EnvVarsKey`, channel config, enabled protocols, tool build env, backend overrides) at construction (`builder.rs:346-363`, `421-547`).
- The compute engine dedupes/caches completed key values for the engine lifetime, but current injected keys are single-write because there is no invalidation mechanism (`engine.rs:135-179`; `injected.rs:27-43`).
- Existing filesystem-dependent keys/caches are already localized: input glob walks are compute keys cached by `(root, patterns, markers, exclude_hidden)` (`input_glob_set_walk.rs:92-139`); backend metadata cache keys include manifest/backend/config hashes and may fingerprint mutable backend binaries (`input_hash.rs:21-63`, `build_backend_metadata/mod.rs:220-260`); `clear_filesystem_caches()` currently only clears `GlobHashCache`, not compute-engine graph entries (`command_dispatcher/mod.rs:303-310`).
- CLI `run` and `shell` own argument/config merging, progress/UI, platform decisions, task graph construction, process execution, shell selection, signal behavior, dry-run, and interactive disambiguation (`run.rs:124-143`, `223-279`, `402-455`; `shell.rs:324-365`).

## Architecture

Pixi currently rebuilds three layers per CLI process:

1. **Workspace/manifest/config layer**: discover workspace, read `pixi.toml`/`pyproject.toml`, parse manifests, load global/workspace config, initialize `Workspace` caches.
2. **Lock/prefix consistency layer**: read `pixi.lock`, build a resolver, compute lock satisfiability/outdated targets, create per-invocation derived data and build caches, then quick-validate prefixes via marker files.
3. **Build/solve/source layer**: construct a fresh `CommandDispatcher` and `ComputeEngine`, inject snapshots, then compute source checkouts, source metadata, backend discovery, glob walks, solves, and installs.

A daemon seam fits best around a **workspace-scoped session** containing a stable `Workspace`/manifest snapshot plus a long-lived `CommandDispatcher`/`ComputeEngine` and selected `LockFileDerivedData`-like caches. The existing compute graph is useful for keeping source metadata, glob walks, backend discovery, solve inputs, and installed-environment computations warm, but it lacks invalidation. Filesystem watching therefore needs either graph invalidation by key/dependency or coarse engine/session replacement.

Filesystem invalidation can map to existing abstractions roughly as:

- manifest/config changes (`pixi.toml`, `pyproject.toml`, package manifests, config files) invalidate the `Workspace` snapshot and anything derived from environment/platform/task definitions;
- `pixi.lock` changes invalidate loaded lock file, `LockFileResolver`, outdated result, and prefix hash decisions;
- `.pixi/envs/**/conda-meta/pixi_env.json` or prefix contents invalidate prefix quick-validation / activation-fingerprint assumptions;
- source-tree changes invalidate `InputGlobSetWalkKey`, `GlobHashCache`, source metadata/build artifact freshness, and source-package prefix acceptance;
- backend executable/path changes invalidate backend metadata keys where `BackendCacheStrategy::Fingerprint` applies;
- environment variable changes are currently captured once in dispatcher injected `EnvVarsKey`; daemon requests that depend on env/config should treat env snapshots as request inputs or require a new engine generation.

## Start Here

Start with `crates/pixi_core/src/lock_file/update.rs` (`Workspace::update_lock_file`). It is the central per-invocation rebuild path and shows where lockfile loading, dispatcher creation, satisfiability checks, and derived-data caches converge.

## Architectural Implications

- A daemon should probably be **one per workspace root**, not global-only: cache dirs include workspace `.pixi`, the dispatcher builder is workspace-derived, and invalidation naturally keys off manifest root.
- Keeping a long-lived `CommandDispatcher` is attractive, but unsafe without invalidation because compute-engine results and injected values are immutable for engine lifetime.
- Short-term safe design could use **generation-based coarse invalidation**: replace the workspace session/dispatcher/engine on manifest/env-var/config changes; clear or replace filesystem-sensitive compute state on source/prefix/lock changes.
- Longer-term design should add compute-engine invalidation using recorded dependency graph edges; existing `Key::equality` is documented for future early cutoff but not active invalidation.
- CLI should remain responsible for UX and local process concerns: parsing flags, deciding locked/frozen/no-install behavior, progress rendering, prompts/disambiguation, spawning tasks/shells, signal handling, and stdout/stderr behavior. The daemon should answer stateful queries/operations such as “load workspace snapshot”, “ensure lock up to date”, “ensure prefix”, “compute activation env”, “resolve/build source metadata”, and “return task graph data”, while the CLI executes user commands locally.

## Risks / Open Questions

- `EnvVarsKey` snapshots all environment variables once; daemon APIs need a policy for per-request env inputs vs daemon-start env.
- `clear_filesystem_caches()` does not clear compute-engine cached `InputGlobSetWalkKey` values, so merely calling it after file changes would be insufficient for a daemon.
- Source packages intentionally bypass some quick-validation shortcuts; a daemon watcher could make that faster but must avoid accepting stale built artifacts.
- `Workspace` contains mutable-ish cached activation env vars and network/gateway cells; if reused across requests, concurrency and per-request config overrides need explicit boundaries.

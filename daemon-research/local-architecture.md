# Code Context

Source was inspected from git object `3b14a4892edc229275217ce1d168edf6e11836bd` because this worktree has deleted sources. References below are `git show 3b14...:<path>` line numbers.

## Files Retrieved
1. `crates/pixi/src/main.rs` (lines 6-50) - binary entrypoint: creates Tokio runtime on a larger-stack thread and calls `pixi_cli::execute()`.
2. `crates/pixi_cli/src/lib.rs` (lines 57-200, 235-279, 353-391) - top-level Clap args/commands and dispatcher from CLI command enum to per-command handlers.
3. `crates/pixi_command_dispatcher/src/lib.rs` (lines 1-39, 68-145) - crate-level architecture and public API surface for the compute-backed dispatcher.
4. `crates/pixi_command_dispatcher/src/command_dispatcher/mod.rs` (lines 51-174, 182-221) - `CommandDispatcher` handle and shared data: gateway, cache dirs, package cache, resolvers, semaphores, engine.
5. `crates/pixi_command_dispatcher/src/command_dispatcher/builder.rs` (lines 46-83, 346-548) - builder inputs and `finish()` wiring of cache dirs, gateway, package cache, engine data, injected keys, reporters.
6. `crates/pixi_compute_engine/src/lib.rs` (lines 1-7, 60-76, 183-229, 255-260) - generic incremental engine semantics, cancellation, global data, injected-key immutability, dependency graph introspection.
7. `crates/pixi_core/src/workspace/mod.rs` (lines 760-848, 850-916) and `crates/pixi_core/src/workspace/repodata.rs` (lines 5-18) - workspace-owned authenticated client, repodata gateway, and pre-filled command dispatcher builder.
8. `crates/pixi_config/src/lib.rs` (lines 693-742, 1308-1335, 1566-1586, 2736-2747) and `crates/pixi_auth/src/lib.rs` (lines 14-71) - config/auth/network knobs used by long-lived services.
9. `crates/pixi_core/src/lock_file/update.rs` (lines 236-340, 518-590) and `crates/pixi_core/src/environment/mod.rs` (lines 689-790) - lock/update/install flow creates a dispatcher per operation and carries it in `LockFileDerivedData`.
10. `crates/pixi_cli/src/install.rs` (lines 83-162), `crates/pixi_cli/src/run.rs` (lines 127-238, 402-455), `crates/pixi_cli/src/exec.rs` (lines 75-125, 216-367) - primary install/run/exec flows and where daemonization would help or not.
11. `crates/pixi_command_dispatcher/src/keys/solve_conda.rs` (lines 220-323) and `crates/pixi_command_dispatcher/src/solve_conda/mod.rs` (lines 21-86, 109-340) - repodata fetch then blocking rattler solve.
12. `crates/pixi_command_dispatcher/src/cache/mod.rs` (lines 1-33), `crates/pixi_compute_cache_dirs/src/lib.rs` (lines 1-18) - on-disk cache inventory and engine-tracked cache-dir resolution.
13. `crates/pixi_command_dispatcher/src/keys/source_metadata.rs` (lines 78-180), `keys/source_build.rs` (lines 60-152, 226-268, 320-370), `build_backend_metadata/mod.rs` (lines 118-126, 946 area), `instantiate_backend_key.rs` (lines 79-190) - source/build/backend resolution chain.
14. `crates/pixi_compute_sources/src/{lib.rs,ext.rs,git.rs,url.rs}` (grep hits around `SourceCheckoutExt`, `CheckoutGit`, URL keys) - source checkout abstraction for git/url/path sources.
15. `crates/pixi_build_backend/src/server.rs` (lines 22-66, 68-130), `crates/pixi_build_backend/src/cli.rs` (lines 13-44), `crates/pixi_build_backend/src/protocol.rs` (lines 8-46), `crates/pixi_build_frontend/src/backend/json_rpc.rs` (lines 42-170) - existing JSON-RPC server/client for build backends only.

## Key Code

### CLI startup and dispatch
```rust
// crates/pixi/src/main.rs:33-40
let main2 = move || {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(Box::pin(pixi_cli::execute()))
};
```
`pixi_cli::execute()` parses Clap, configures miette/logging/progress, then calls `execute_command(command, &global_options)` (`crates/pixi_cli/src/lib.rs:235-279`). Top-level commands are static variants plus `External(Vec<String>)` for `pixi-*` extensions (`crates/pixi_cli/src/lib.rs:153-200`). There is no `Daemon` variant currently.

### Dispatcher / compute engine
`pixi_command_dispatcher` is already the closest in-process daemon boundary. It is documented as a thin handle over a generic incremental computation engine: operations are `Key`s; `ComputeEngine` dedups concurrent requests and caches results; dependencies are tracked by `ComputeCtx::compute`; shared resources live in a typed `DataStore` (`crates/pixi_command_dispatcher/src/lib.rs:18-32`).

`CommandDispatcherData` contains the long-lived resources a daemon would want to amortize: `Gateway`, `BuildBackendMetadataCache`, `GitResolver`, `UrlResolver`, `CacheDirs`, `LazyClient`, `PackageCache`, semaphores for git/url/solves/builds/I/O, reporters, and `WorkspaceEnvRegistry` (`crates/pixi_command_dispatcher/src/command_dispatcher/mod.rs:91-174`).

Builder `finish()` currently snapshots env vars once, creates `PackageCache`, default `Gateway`, resolvers, semaphores, registers global data, and injects immutable keys like `CacheDirsKey`, `EnvVarsKey`, `ChannelConfigKey`, `EnabledProtocolsKey`, `ToolBuildEnvironmentKey`, and `BackendOverrideKey` (`builder.rs:346-548`). Important daemon constraint: compute engine injected keys are single-write and there is no invalidation mechanism (`pixi_compute_engine/src/lib.rs:216-229`).

### Workspace integration
Workspace exposes `command_dispatcher_builder()` (`crates/pixi_core/src/workspace/mod.rs:783-848`). It resolves:
- global cache root plus per-workspace `.pixi` cache anchor,
- workspace root dir,
- authenticated network client,
- repodata gateway,
- max concurrent downloads/solves,
- backend overrides,
- channel config,
- link-script/link-mode policy,
- tool platform and virtual packages.

`Workspace::repodata_gateway()` lazily creates a gateway from config with authenticated client and download semaphore (`crates/pixi_core/src/workspace/repodata.rs:5-18`). This is currently per-`Workspace` process memory, not shared across CLI invocations.

### Network/auth/config
Config CLI and file settings cover auth-file override, concurrent downloads/solves, TLS no-verify/root certs, cache dirs, mirrors, and repodata per-channel config (`crates/pixi_config/src/lib.rs:693-742`, `1308-1335`, `1566-1586`, `2736-2747`). `pixi_auth::get_auth_store()` honors `RATTLER_AUTH_FILE`, pixi `authentication_override_file`, then default keyring/credential storage (`crates/pixi_auth/src/lib.rs:14-71`). `build_lazy_reqwest_clients()` creates a `LazyReqwestClient` plus rattler `LazyClient` with middleware stack (`crates/pixi_utils/src/reqwest.rs:266-286`).

### Install / lock / solve path
`pixi install` locates a workspace, selects environment(s), builds an `InstallFilter`, then calls `get_update_lock_file_and_prefixes(...)` (`crates/pixi_cli/src/install.rs:83-162`). That calls `Workspace::update_lock_file()` and then `LockFileDerivedData::prefix(...)` for each environment (`crates/pixi_core/src/environment/mod.rs:689-790`).

`Workspace::update_lock_file()` creates a fresh `CommandDispatcher` from the workspace builder, optionally registers progress reporters, then stores it in `LockFileDerivedData` alongside package cache and lock file (`crates/pixi_core/src/lock_file/update.rs:282-303`, `563-590`). This fresh-per-command construction is a direct daemon opportunity.

`keys/solve_conda.rs` derives all gateway match specs, fetches recursive repodata with `ctx.global_data().gateway()`, then calls `ctx.solve_conda(conda_spec)` (`crates/pixi_command_dispatcher/src/keys/solve_conda.rs:220-323`). `SolveCondaEnvironmentSpec::solve_on_blocking_pool()` spawns a blocking rattler solve and combines binary repodata with source/dev-source pseudo-records (`solve_conda/mod.rs:109-340`).

### Run / task flow
`pixi run` locates workspace, updates lock file, pins run platform, builds a `TaskGraph`, optionally installs missing prefixes via `lock_file.prefix(...)`, clears dispatcher filesystem caches after installs/tasks can change files, gets activation env, then runs scripts with `deno_task_shell` (`crates/pixi_cli/src/run.rs:127-238`, `402-455`). Long-lived daemon must preserve child-process signal/exit semantics and not own task execution unless explicitly designed.

### Exec flow
`pixi exec` is mostly independent of the command dispatcher today. It creates a config and reqwest client, hashes specs/channels/platform to a cached prefix under global cache, uses `EnvironmentLock` for cross-process prefix locking, builds a standalone `Gateway`, fetches repodata, solves directly with rattler, and installs with `rattler::install::Installer` (`crates/pixi_cli/src/exec.rs:75-125`, `216-367`). A daemon could add value here by sharing gateway/client/repodata caches, but this path would need refactoring into dispatcher keys or a daemon-specific service.

### Source/build resolution
Source handling flows through compute keys:
- `SourceCheckoutExt` dispatches git/url/path source specs to compute-source keys (`crates/pixi_compute_sources/src/ext.rs`, `git.rs`, `url.rs`).
- `SourceMetadataKey` checks out source, builds `BuildBackendMetadataSpec`, computes `BuildBackendMetadataKey`, and selects records for a package (`crates/pixi_command_dispatcher/src/keys/source_metadata.rs:78-180`).
- `BuildBackendMetadataKey` dedups backend `conda/outputs` metadata, instantiating backend through `InstantiateBackendKey` when needed.
- `SourceBuildKey` checks out pinned sources, probes artifact/workspace caches, instantiates backend, extracts run exports through gateway, and builds `.conda` artifacts (`keys/source_build.rs:60-152`, `226-268`, `320-370`).

### Existing JSON-RPC/server code
The repo already has JSON-RPC but only for build backend protocol, not a Pixi daemon. Workspace `Cargo.toml` includes `axum`, `jsonrpc-core`, `jsonrpc-http-server`, `jsonrpc-stdio-server`; `pixi_build_backend::Server` exposes stdio or loopback HTTP JSON-RPC with initialize/negotiate/conda_outputs/conda_build_v1 (`server.rs:22-130`, `cli.rs:13-44`, `protocol.rs:8-46`). Frontend spawns a backend process and talks over JSON-RPC stdio using `jsonrpsee` (`pixi_build_frontend/src/backend/json_rpc.rs:145-170`). This provides transport precedent, but semantics are backend-specific and stateful per backend process.

## Architecture

Current shape:

`pixi` binary -> `pixi_cli::execute()` -> Clap command handler -> `WorkspaceLocator`/`Config` -> workspace methods -> fresh `CommandDispatcher` -> `ComputeEngine` keys -> rattler gateway/solver/installer/build backends/cache/source checkouts.

The strongest daemon boundary is **below CLI parsing and above `CommandDispatcher`/workspace operations**:
- CLI remains responsible for UX, progress, signals, child execution, and external subcommands.
- Daemon owns long-lived `CommandDispatcher` instances or resource pools keyed by workspace root + config fingerprint + env snapshot + tool platform + backend override + channel config + auth/TLS/mirror settings.
- Requests ask daemon for solve/update/install/source metadata/build prep results, while CLI performs terminal-sensitive task execution and process exit mirroring.

Alternative boundaries:
1. **Resource daemon**: expose cached gateway/client/package-cache/source-checkout/metadata services; leave most operations in-process. Lower risk, less benefit.
2. **Dispatcher daemon**: expose command-dispatcher operations (solve pixi env, install env, source metadata/build, instantiate tool env). Best match to current abstractions.
3. **Full CLI daemon**: send parsed `Command` or raw argv to daemon. Highest risk: terminal/progress/signal/interactive auth/task execution are tightly CLI-process-bound.

## Likely Daemon Boundary Options

1. **Per-workspace dispatcher cache**
   - Key daemon sessions by workspace root and a stable config/env fingerprint.
   - Reuse `Workspace::command_dispatcher_builder()` logic, but move `finish()` into daemon session creation.
   - Benefits: compute dedup/caches survive multiple CLI invocations; source/build/solve graph can be reused.
   - Risk: no compute invalidation; must destroy/recreate session when manifest, lock file, config, auth, env vars, cache env overrides, platform overrides, backend overrides, or virtual package assumptions change.

2. **Global gateway/repodata daemon**
   - Centralize `Gateway`, `LazyClient`, auth middleware, mirror/TLS config, and package cache.
   - Benefits `install`, `run`, `search`, `exec`; simpler API.
   - Risk: still duplicates compute graph/source metadata unless dispatcher is also daemonized.

3. **Build/source daemon**
   - Reuse source checkouts, backend metadata cache, build backend processes, and artifact/workspace caches.
   - Benefits source-heavy workspaces.
   - Risk: build backend JSON-RPC processes are already long-ish lived within a dispatcher; daemon must manage backend process lifetimes and stderr/progress routing.

4. **Task execution helper**
   - Daemon only prepares/installs env and returns activation env; CLI executes tasks locally.
   - Avoids daemon owning terminal/signal behavior (`pixi run` currently mirrors child exit and handles Ctrl-C in-process).

## Integration Points

- Add `Command::Daemon` in `crates/pixi_cli/src/lib.rs` and a new `crates/pixi_cli/src/daemon.rs` if the CLI starts/stops/statuses a service.
- New crate likely needed, e.g. `crates/pixi_daemon`, depending on `pixi_core`, `pixi_command_dispatcher`, `pixi_config`, and transport crate (`axum` already in workspace, JSON-RPC crates already present).
- Reuse `WorkspaceLocator` and `Workspace::command_dispatcher_builder()` for session creation to avoid duplicating config/auth/cache policy.
- Expose request handlers around existing public dispatcher methods: `install_pixi_environment`, `build_backend_metadata`, `instantiate_backend`, source metadata/build keys, solve environment keys. Avoid exposing raw `Key` across wire unless there is a stable schema.
- Progress/reporting needs a daemon-aware reporter replacing `TopLevelProgress::register_with(builder)` with streamed events.
- `pixi exec` would need either a daemon endpoint for standalone temporary env solve/install or refactoring into dispatcher/ephemeral-env APIs.

## Constraints / Risks

- **No invalidation in `ComputeEngine`**: injected keys are immutable and results cached. Long-lived daemon must version/fingerprint sessions and recreate them aggressively.
- **Env snapshot is captured at dispatcher creation** (`builder.rs:360-363`, injects `EnvVarsKey`). Changes to `PIXI_*`, `CONDA_OVERRIDE_*`, cache overrides, proxies, auth env, etc. need session invalidation.
- **Auth/config freshness**: keyring/file tokens and global config can change via `pixi auth`/`pixi config`; daemon needs watch, TTL, or explicit reload.
- **Workspace mutations**: `pixi.toml`, `pixi.lock`, source files, glob inputs, and task outputs can change between requests. Existing `run` clears filesystem caches after task effects (`run.rs:432-433`); daemon must expose equivalent cache clearing and possibly file watching.
- **Terminal semantics**: progress bars, prompts, Ctrl-C, child exit status, shell activation, and external `pixi-*` commands are CLI-process concerns.
- **Concurrency and locks**: existing prefix/environment locks protect cross-process installs (`pixi exec` uses `EnvironmentLock`; dispatcher install path also has locks). Daemon must coordinate with non-daemon pixi processes and older versions.
- **Security**: local daemon transport must authenticate same-user clients and avoid exposing auth tokens, env vars, local filesystem operations, or arbitrary command execution to other users.
- **Windows/macOS/Linux lifecycle**: socket path/named pipe choice, service autostart, stale lock cleanup, and per-user vs per-workspace daemon model need explicit design.
- **Build backend process lifetime**: existing backend JSON-RPC protocol is stateful after initialize; daemon may need lifecycle/idle timeout and stderr/progress multiplexing.

## Start Here
Open `crates/pixi_core/src/workspace/mod.rs` around `command_dispatcher_builder()` first. It is the seam where workspace/config/auth/cache/network state is converted into a reusable `CommandDispatcherBuilder`, and it determines what a daemon session must fingerprint and recreate.

## Clarification Questions

1. Should `pixi daemon` be per-user global, per-workspace, or hybrid with per-workspace sessions inside one user daemon?
2. Is the first production goal faster repeated `pixi run/install` in one workspace, shared repodata/cache across all workspaces, or background task/build execution?
3. Should CLI commands transparently use the daemon by default, or only when explicitly enabled/configured?
4. What invalidation freshness is acceptable: exact file/env/config watching, request-supplied fingerprints, TTLs, or manual `pixi daemon restart`?
5. Must the daemon support `pixi exec`, or can initial scope focus on workspace commands that already use `CommandDispatcher`?
6. Should task child processes run in the CLI process (safer terminal semantics) or in the daemon (background/job-control features)?
7. What local transport and auth model is desired: Unix socket/named pipe, loopback HTTP, stdio child daemon, or JSON-RPC over one of these?
8. How should progress and diagnostics stream over the daemon boundary: structured events compatible with existing reporters, raw logs, or a simpler request/response model initially?

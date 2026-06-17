# Pixi daemon: prototype-to-production planning frame

`design/pixi-daemon.md` is empty in this worktree. The frame below is derived from local source at commit `3b14a4892edc229275217ce1d168edf6e11836bd`, using `git show`/`git grep` because this checkout currently lacks `crates/`.

## Repo facts that should shape the daemon

- Pixi is currently a command-dispatching CLI. `crates/pixi_cli/src/lib.rs` defines top-level commands (`Add`, `Install`, `Run`, `Shell`, `Task`, `Workspace`, `Global`, etc.) and maps each directly to an async handler in `execute_command`.
- Workspace discovery is an explicit, path-sensitive operation. `WorkspaceLocator::for_cli()` enables warnings and environment-variable-sensitive discovery; `DiscoveryStart` can be current dir, search root, explicit manifest, or workspace registry name (`crates/pixi_core/src/workspace/discovery.rs`).
- Install/run flows already combine lock updating, prefix installation, platform selection, and reporting:
  - `pixi install` locates workspace, chooses environment(s), builds an `InstallFilter`, and calls `get_update_lock_file_and_prefixes` with `UpdateMode::Revalidate` and `max_concurrent_solves` from config (`crates/pixi_cli/src/install.rs`).
  - `pixi run` locates workspace, chooses environment/platform, updates lock file, installs on demand unless disabled, then runs task/executable in an activated environment (`crates/pixi_cli/src/run.rs`).
  - `pixi shell` similarly updates/installs and computes activation variables before spawning an interactive shell (`crates/pixi_cli/src/shell.rs`).
- There is already a library-oriented API seam in `crates/pixi_api`: `DefaultContext` and `WorkspaceContext` expose workspace operations (init, package add/remove, list, task CRUD, search, reinstall) behind an `Interface` trait for prompts/logging (`crates/pixi_api/src/context.rs`, `interface.rs`). This is a likely substrate for daemon business operations, but it does not cover all CLI journeys such as process execution/session lifecycle.
- Pixi already has a JSON-RPC precedent in build backends:
  - Backend server supports stdio and localhost HTTP, has `negotiate_capabilities`, `initialize`, then method calls (`crates/pixi_build_backend/src/server.rs`, `protocol.rs`).
  - Frontend negotiates capabilities before initialization and uses long request timeouts for long-running work (`crates/pixi_build_frontend/src/backend/json_rpc.rs`).
  - Build types document API versioning/backward compatibility by adding optional fields and requiring old/new clients to populate/read fallbacks (`crates/pixi_build_types/src/procedures/conda_outputs.rs`).
- Cache and workspace boundaries are non-trivial:
  - Global cache root resolution honors `PIXI_CACHE_DIR`, `RATTLER_CACHE_DIR`, `[cache.root]`, XDG, and rattler defaults (`crates/pixi_config/src/lib.rs`). Per-kind cache dirs cover conda packages, repodata, PyPI wheels, pypi mapping, exec envs, build-tool envs, detached envs, with network-filesystem redirection for some kinds.
  - Workspace-local state is anchored at `<workspace>/.pixi` by default; detached-environments can move effective `pixi_dir()` to a project-specific detached path. Environment prefixes live under `environments_dir()`, task and activation caches under `pixi_dir()` (`crates/pixi_core/src/workspace/mod.rs`).
  - The command dispatcher receives both a global cache anchor and workspace anchor through `CacheDirs::new(root).with_workspace(workspace_dir)` (`crates/pixi_core/src/workspace/mod.rs`, `crates/pixi_compute_cache_dirs/src/cache_dirs.rs`).
  - Source-build workspace caches have cross-process `.lock` files and correctness-sensitive keys (`crates/pixi_command_dispatcher/src/cache/workspace.rs`). Environment prefixes have a cross-process exclusive `EnvironmentLock` and crash/interruption marker (`crates/pixi_utils/src/environment_lock.rs`).

## Primary user journeys to support first

1. **Fast repeated `pixi run` / task execution in an existing workspace**
   - User invokes `pixi run test` repeatedly from an editor, shell, or CI-like loop.
   - Daemon can keep warm workspace metadata, config, repodata/client state, and possibly activation/task resolution artifacts.
   - CLI remains responsible for terminal UX, argument parsing, stdio attachment, exit-code propagation, and direct fallback if daemon unavailable.

2. **Editor/IDE workspace introspection and mutations**
   - List environments, tasks, platforms, channels, packages; add/remove dependencies or tasks; initialize workspace.
   - This aligns with `pixi_api::WorkspaceContext` and should be a low-risk early slice because it is request/response oriented and usually does not need child-process stdio ownership.

3. **On-demand environment prepare/install**
   - User asks CLI/editor to ensure an environment is ready for a platform with standard `locked/frozen/no-install` semantics.
   - Daemon coordinates concurrent requests for the same workspace/environment/platform but must preserve existing cross-process lock correctness.

4. **Activation materialization for shell/editor integrations**
   - Compute activation env vars or shell snippets for a workspace/environment without necessarily spawning an interactive shell.
   - Useful to editors and shell hooks; should respect the experimental activation cache and input env vars.

5. **Status/diagnostics/control**
   - `pixi daemon status`, logs, health, active operations, cached workspaces, protocol version, and `stop/restart` controls.
   - Needed before production rollout, even if hidden/experimental initially.

## Responsibilities: daemon vs CLI

### Daemon should own

- Workspace-scoped coordination for expensive, repeatable operations: discovery result caching, manifest/config watching or invalidation, lock-file update/solve/install orchestration, package search using configured clients, activation computation, and operation de-duplication.
- A stable local RPC API with explicit protocol negotiation, request IDs, structured errors, progress events, cancellation, and compatibility rules.
- Lifecycle state: known workspace handles, in-flight operation registry, cache invalidation, health/metrics, log files, and graceful shutdown.
- Correct serialization around workspace mutations and environment installs, while reusing existing lock mechanisms rather than bypassing them.

### CLI should continue to own

- User-facing CLI parsing, help, color/progress policy, terminal detection, prompt policy, and exact text/exit-code contract.
- Process/session attachment for `run`, `exec`, and interactive `shell` unless/until a later design deliberately moves child process supervision into the daemon.
- Safe fallback path: if daemon is absent, incompatible, unhealthy, or disabled, execute the current in-process implementation.
- Explicit writes to manifests/lock files should still appear as CLI-driven user actions; daemon may perform them but CLI must decide when it is allowed by flags such as `--locked`, `--frozen`, `--no-install`, and confirmation settings.

## Process and lifecycle model

Recommended initial model: **per-user, on-demand local daemon with per-workspace state**, not per-workspace daemon and not system service.

- Start lazily from CLI/editor on first eligible request; discover via a socket/metadata file under a user-scoped runtime directory. Avoid binding public network interfaces.
- Single daemon process can serve multiple workspaces but should internally partition state by canonical workspace root plus config/discovery inputs.
- Use idle timeout and explicit `stop`; keep production option for long-lived startup by shell/editor integration.
- Enforce executable/protocol compatibility at connect time: client pixi version, daemon pixi version, protocol version, feature set, and minimum supported API.
- Prefer a local Unix domain socket / Windows named pipe for production. Localhost HTTP is useful for debugging because build backend precedent exists, but production should avoid unauthenticated TCP by default.
- Supervision should be simple initially: CLI spawns detached daemon, waits for readiness/handshake, and falls back on timeout. Later packages can add launchd/systemd/user service integration if demand exists.

## API surface and versioning

Recommended slices:

1. **Control API**: `health`, `shutdown`, `capabilities`, `version`, `listWorkspaces`, `operationStatus`, `cancel`.
2. **Workspace API**: `openWorkspace`/`resolveWorkspace`, `workspaceSummary`, `listEnvironments`, `listTasks`, `listPackages`, `search`, `add/remove dependency`, `add/remove task`.
3. **Environment API**: `ensureEnvironment`, `activation`, `prefixInfo`, `cleanActivationCache`.
4. **Event stream**: progress, warnings, prompts-needed, operation-complete, cancellation.

Versioning rules should mirror build-backend lessons:

- Start every connection with `negotiateCapabilities` and `initialize`/`openWorkspace` rather than assuming method availability.
- Version the protocol independently from pixi CLI version, but report both.
- Add fields as optional with defaults and fallback fields when changing shape; do not remove or repurpose fields within a major protocol line.
- Use structured errors with stable codes plus diagnostic text. Keep miette-style rich messages for humans, but machine clients need codes like `workspace_not_found`, `lockfile_outdated`, `incompatible_daemon`, `operation_cancelled`, `install_interrupted`, `permission_denied`.
- Treat long-running operations as operation handles with progress/events and cancellation, not as opaque blocking RPC calls only.

## Cache and workspace boundaries

Production daemon must preserve these boundaries:

- **Workspace identity**: root path plus manifest file, effective config source (`--no-config`/`--config-file`), and env-sensitive discovery inputs. Symlink/canonicalization behavior matters because source tests preserve symlink workspace location.
- **Global vs workspace cache**: global caches (`pkgs`, repodata, PyPI wheels/mapping, exec/build-tool/detached envs) remain shared according to `pixi_config`; workspace state remains under effective `pixi_dir()` or detached path.
- **Per-workspace invalidation**: manifest(s), `pixi.lock`, local `.pixi/config.toml`, selected global config layer, environment variables that influence discovery/platform/proxy/cache, and activation input env vars can invalidate cached daemon state.
- **Concurrency**: use daemon-level de-duplication as an optimization only. Existing file locks and fingerprint markers remain the source of truth for safety across daemon/non-daemon pixi processes.
- **Security/privacy**: auth tokens and private-channel URLs may flow through configured clients. Do not expose daemon beyond current user; do not log credentials; ensure socket/pipe permissions are user-only.

## Failure modes and fallback

Plan for these from the first production milestone:

- Daemon not installed/running: CLI runs existing in-process path.
- Daemon too old/new or protocol mismatch: CLI falls back and emits debug-level reason unless user asked for daemon explicitly.
- Daemon startup timeout/readiness failure: kill orphan if possible, then fallback.
- Operation panic/crash or daemon exits mid-request: CLI retries in-process only when operation is known safe/idempotent; otherwise report a clear failure and point to logs.
- Lock contention: daemon reports waiting/progress using existing lock callbacks; cancellation should stop waiting when possible without corrupting prefix state.
- Interrupted install detected by `EnvironmentLock`: force reinstall path as current code intends.
- Stale workspace/config cache: detect by file mtimes/content hashes or conservative invalidation; provide `daemon reload`/`clear-workspace` escape hatch.
- Network/proxy/cache errors: preserve current CLI diagnostics; do not hide behind generic RPC errors.
- Interactive prompts: daemon must not block invisibly. Either CLI supplies a prompt policy/callback or operation fails with `prompt_required` for non-interactive clients.
- Child process execution: initial daemon should not own terminal children; if future daemon-owned execution is added, it needs explicit stdio streaming, signal forwarding, pty handling, and exit-code contracts.

## Recommended initial architecture slices

1. **Daemon shell + protocol foundation**
   - Local transport, handshake, capabilities, health/status, structured errors, logging, startup readiness, shutdown.
   - No workspace mutations yet.

2. **Workspace read API over existing model**
   - Resolve/open workspace, return summary, list environments/tasks/platforms/packages/search.
   - Use `pixi_api` where possible; prove non-CLI interface path works.

3. **Operation framework**
   - Long-running operation handles, progress events, cancellation token, per-workspace operation registry, CLI fallback on connection failure.
   - Needed before install/solve work moves behind RPC.

4. **Ensure environment**
   - Daemon wraps existing lock/update/install pipeline for one environment/platform using current `LockFileUsage` semantics.
   - Reuse existing file locks; add daemon-level request coalescing for identical workspace/env/platform/filter requests.

5. **Activation service**
   - Return activation env map/shell script metadata for known shells, respecting input env and activation cache semantics.
   - Keep `pixi shell` process spawning in CLI.

6. **Mutating workspace operations**
   - Add/remove dependency, task and channel/platform operations using `WorkspaceContext` patterns and strict serialization against concurrent installs/solves.

7. **Production hardening slice**
   - Socket permission checks, logs/status UX, telemetry/metrics if project policy allows, compatibility tests, stress/concurrency tests, docs, feature flags/defaults.

## Non-goals for the first production path

- No remote daemon or multi-user service.
- No daemon-owned interactive shell or pty/session manager in the first production milestone.
- No replacement of existing lock files, prefix file locks, or cache layouts.
- No new global database as source of truth for workspace state.
- No redesign of solver/install algorithms.
- No broad plugin ecosystem or external API stability promise beyond the local pixi client/editor integration contract.
- No mandatory daemon dependency for normal CLI operation until fallback and compatibility have been proven.

## Unknowns / decisions to resolve

- Exact current daemon prototype state is not visible in this checkout/design file; verify whether any branch/PR already contains a daemon crate or command.
- Transport choice for production: Unix socket/Windows named pipe vs localhost HTTP for all platforms.
- Where daemon runtime metadata/logs live (`XDG_RUNTIME_DIR`, pixi home, cache dir, Windows equivalents) and how stale sockets are cleaned.
- Whether daemon and CLI are always same executable/version or daemon can outlive upgrades. This affects auto-restart and compatibility policy.
- Which clients besides CLI are in scope for the first stable protocol (official editor extension? language server?).
- Prompt/cancellation semantics for operations currently relying on CLI `Interface` and terminal state.
- File watching vs conservative per-request stat/hash validation for manifests/config/lock files.
- Whether authentication state in rattler/pixi config needs explicit daemon refresh/logout handling.

## Suggested milestone plan

### Milestone 0: Prototype audit and architecture decision record

- Inventory any hidden/prototype daemon code and write the minimal ADR: process model, transport, protocol versioning, fallback policy, security assumptions.
- Acceptance: team agrees on per-user local daemon, local-only transport, capability handshake, and CLI fallback invariants.

### Milestone 1: Experimental daemon control plane

- Add hidden/experimental `pixi daemon` controls: start/status/stop, health, version/capabilities, logs location.
- CLI can connect, detect incompatibility, and fall back.
- Acceptance: cross-platform smoke tests for startup, stale socket cleanup, permission checks, protocol mismatch, and no-daemon fallback.

### Milestone 2: Workspace read/introspection API

- Implement open/resolve workspace and read-only methods for environments/tasks/platforms/packages/search.
- Reuse `pixi_api`/`WorkspaceLocator`; validate cache invalidation when manifest/config changes.
- Acceptance: editor-style client can list workspace data; CLI output remains unchanged when routed through daemon for selected read-only commands behind a flag.

### Milestone 3: Operation framework + progress/cancel

- Introduce operation handles and event stream. Support progress forwarding and cancellation for synthetic and read/search operations first.
- Acceptance: client can start, observe, cancel, and query operations; daemon survives client disconnects according to defined policy.

### Milestone 4: Ensure environment behind daemon

- Implement `ensureEnvironment` for one environment/platform with current `locked/frozen/no-install` semantics and existing prefix locks.
- Add request coalescing and lock contention reporting.
- Acceptance: `pixi install`/`pixi run --no-exec test-path` style internal checks can use daemon experimentally; concurrent clients do not corrupt prefixes; fallback remains correct.

### Milestone 5: Activation service

- Return activation environment/script data for shell-hook/editor consumers; honor activation-cache inputs and invalidation.
- Acceptance: shell-hook/editor flow can request activation without a full CLI install path when env is ready; stale activation cache tests pass.

### Milestone 6: Controlled beta rollout

- Feature flag/config opt-in (`PIXI_DAEMON=1` or experimental config), docs, troubleshooting, status command, compatibility matrix.
- Collect performance and reliability data for repeated run/list/install journeys.
- Acceptance: production readiness gates below are met for opt-in users.

## Production readiness gates

- **Correctness parity**: daemon-routed commands produce the same manifest/lock/prefix outcomes as in-process CLI across representative install/run/shell-hook/list/add/remove cases.
- **Fallback safety**: all daemon connection/startup/version failures fall back unless user explicitly requested daemon-only behavior.
- **Concurrency safety**: stress tests with multiple CLI processes against same workspace/env show no prefix/cache corruption; existing file locks remain effective with mixed daemon and non-daemon clients.
- **Security**: local transport is user-private; no public TCP listener by default; credential redaction in logs; socket/pipe permissions tested.
- **Lifecycle robustness**: startup readiness, idle shutdown, stale socket cleanup, crash recovery, upgrade/restart behavior, and log discovery work on Linux/macOS/Windows.
- **Observability**: status reports version, pid, uptime, active operations, known workspaces, and recent error/log location.
- **Compatibility discipline**: protocol negotiation tests cover older/newer client-daemon pairs and optional-field evolution.
- **Performance evidence**: measured improvement for repeated workspace introspection/run/activation workflows without regressions in cold path.
- **Documentation**: user-facing opt-in/out, troubleshooting, cache boundary notes, and known limitations are documented.
